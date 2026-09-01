// api/ZClawApi.ets — ZClaw 本地Agent API
// 优先使用 Rust libzclaw.so（12工具完整能力），fallback到ArkTS轻量agent loop（5工具）
import http from '@ohos.net.http';
import { Constants } from '../common/Constants';
import { AuthStore } from '../store/AuthStore';
import { preferences } from '@kit.ArkData';
import { LLMStreamCallbacks, LLMResult, AgentMessage, ToolCall, ToolCallFunction } from '../models/Models';
import { getToolDefinitions, executeTool, ToolDefinition } from './ZClawTools';

// think 标签过滤结果
interface ThinkFilterResult {
  content: string;
  thinking: string;
}

// ── Native .so binding (NAPI) ──
// import zclaw native module (compiled from cpp/zclaw_napi.cpp)
// On HarmonyOS this is loaded via napi_module_register
interface ZClawNative {
  isAvailable(): boolean;
  version(): string;
  init(apiUrl: string, apiKey: string, model: string, workspaceDir: string): boolean;
  chat(message: string): number;  // 0=ok, -1=error, -2=busy
  pollChunks(): string;  // JSON array of chunks
  isRunning(): boolean;
  cancel(): boolean;
  getSessions(): string;
  getMessages(sessionId: string): string;
}

// ── Chunk type (from Rust FFI) ──
interface ZClawChunk {
  chunkType: number;  // 0=text,1=tool_call,2=tool_result,3=done,4=error,5=thinking
  name: string;
  args: string;
  result: string;
}

// Try to load native module — if not available, fallback to TS loop
let zclawNative: ZClawNative | null = null;
let nativeInited = false;
try {
  zclawNative = globalThis.requireNapi('zclaw_napi') as ZClawNative;
} catch {
  zclawNative = null;
}

// ── 数据结构 ──
export interface ZClawInstance {
  id: string;
  token_id: number;
  token_key: string;
  model_name: string;
  created_at: number;
}

export interface ZClawMessage {
  id: number;
  role: string;
  content: string;
  thinking?: string;
  created_at: number;
}

export interface ZClawSession {
  id: string;
  instance_id: string;
  title: string;
  model_name: string;
  updated_at: number;
  messages: ZClawMessage[];
}

// SSE streaming delta types
interface StreamDeltaToolCall {
  index: number;
  id: string;
  function: ToolCallFunction;
}

interface StreamDelta {
  content: string;
  reasoning_content: string;
  tool_calls: StreamDeltaToolCall[];
}

interface StreamChoice {
  delta: StreamDelta;
}

interface StreamChunk {
  choices: StreamChoice[];
}

interface NonStreamMessage {
  content: string;
  tool_calls: ToolCall[];
}

interface NonStreamChoice {
  message: NonStreamMessage;
}

interface NonStreamChunk {
  choices: NonStreamChoice[];
}

const STORE_NAME = 'zclaw_store';
const KEY_INSTANCES = 'zclaw_instances';
const keySessions = (instanceId: string) => `zclaw_sessions_${instanceId}`;

function genId(): string {
  return Date.now().toString() + Math.random().toString(36).slice(2, 8);
}

async function getStore(): Promise<preferences.Preferences> {
  const ctx = getContext();
  return await preferences.getPreferences(ctx, STORE_NAME);
}

// ════════════════════════════════════════
//  实例管理
// ════════════════════════════════════════

export class ZClawApi {

  // ===== 实例 CRUD =====

  static async createInstance(tokenId: number, tokenKey: string, modelName: string): Promise<ZClawInstance> {
    const inst: ZClawInstance = {
      id: genId(),
      token_id: tokenId,
      token_key: tokenKey,
      model_name: modelName,
      created_at: Date.now(),
    };
    const instances = await ZClawApi.getInstances();
    instances.push(inst);
    const store = await getStore();
    await store.put(KEY_INSTANCES, JSON.stringify(instances));
    await store.flush();
    return inst;
  }

  static async getInstances(): Promise<ZClawInstance[]> {
    try {
      const store = await getStore();
      const raw = await store.get(KEY_INSTANCES, '');
      if (!raw) return [];
      return JSON.parse(raw as string) as ZClawInstance[];
    } catch {
      return [];
    }
  }

  static async getInstance(id: string): Promise<ZClawInstance | null> {
    const instances = await ZClawApi.getInstances();
    return instances.find(i => i.id === id) ?? null;
  }

  static async deleteInstance(id: string): Promise<boolean> {
    try {
      let instances = await ZClawApi.getInstances();
      instances = instances.filter(i => i.id !== id);
      const store = await getStore();
      await store.put(KEY_INSTANCES, JSON.stringify(instances));
      await store.delete(keySessions(id));
      await store.flush();
      return true;
    } catch {
      return false;
    }
  }

  // ===== 会话管理 =====

  static async getSessions(instanceId: string): Promise<ZClawSession[]> {
    try {
      const store = await getStore();
      const raw = await store.get(keySessions(instanceId), '');
      if (!raw) return [];
      const sessions = JSON.parse(raw as string) as ZClawSession[];
      return sessions.sort((a, b) => b.updated_at - a.updated_at);
    } catch {
      return [];
    }
  }

  static async getSession(instanceId: string, sessionId: string): Promise<ZClawSession | null> {
    const sessions = await ZClawApi.getSessions(instanceId);
    return sessions.find(s => s.id === sessionId) ?? null;
  }

  static async createSession(instanceId: string, title: string, modelName: string): Promise<ZClawSession> {
    const session: ZClawSession = {
      id: genId(),
      instance_id: instanceId,
      title,
      model_name: modelName,
      updated_at: Date.now(),
      messages: [],
    };
    const sessions = await ZClawApi.getSessions(instanceId);
    sessions.unshift(session);
    const store = await getStore();
    await store.put(keySessions(instanceId), JSON.stringify(sessions));
    await store.flush();
    return session;
  }

  static async deleteSession(instanceId: string, sessionId: string): Promise<boolean> {
    try {
      let sessions = await ZClawApi.getSessions(instanceId);
      sessions = sessions.filter(s => s.id !== sessionId);
      const store = await getStore();
      await store.put(keySessions(instanceId), JSON.stringify(sessions));
      await store.flush();
      return true;
    } catch {
      return false;
    }
  }

  static async saveMessage(instanceId: string, sessionId: string, message: ZClawMessage): Promise<void> {
    const sessions = await ZClawApi.getSessions(instanceId);
    const idx = sessions.findIndex(s => s.id === sessionId);
    if (idx < 0) return;
    sessions[idx].messages.push(message);
    sessions[idx].updated_at = Date.now();
    if (message.role === 'user' && sessions[idx].messages.filter(m => m.role === 'user').length === 1) {
      sessions[idx].title = message.content.slice(0, 30);
    }
    const store = await getStore();
    await store.put(keySessions(instanceId), JSON.stringify(sessions));
    await store.flush();
  }

  static async updateSessionModel(instanceId: string, sessionId: string, modelName: string): Promise<void> {
    const sessions = await ZClawApi.getSessions(instanceId);
    const idx = sessions.findIndex(s => s.id === sessionId);
    if (idx < 0) return;
    sessions[idx].model_name = modelName;
    const store = await getStore();
    await store.put(keySessions(instanceId), JSON.stringify(sessions));
    await store.flush();
  }

  // ===== Native .so 优先 =====

  static isNativeAvailable(): boolean {
    return zclawNative !== null && zclawNative.isAvailable();
  }

  static async ensureNative(tokenKey: string, model: string): Promise<boolean> {
    if (!zclawNative || !zclawNative.isAvailable()) return false;
    if (nativeInited) return true;
    const workspaceDir = '/data/storage/el2/base/haps/entry/files/zclaw';
    const ok = zclawNative.init(Constants.BASE_URL, tokenKey, model, workspaceDir);
    nativeInited = ok;
    return ok;
  }

  /** Cancel the ongoing native chat. */
  static cancelNative(): boolean {
    if (!zclawNative || !zclawNative.isAvailable()) return false;
    return zclawNative.cancel();
  }

  /** Check if native chat is running. */
  static isNativeRunning(): boolean {
    if (!zclawNative || !zclawNative.isAvailable()) return false;
    return zclawNative.isRunning();
  }

  // ===== Agent Loop — 核心！ =====
  // 优先Rust .so（完整12工具），fallback到ArkTS轻量loop（5工具）

  static async runAgentLoop(
    tokenKey: string,
    messages: AgentMessage[],
    model: string,
    onChunk: (text: string) => void,
    onThinking: (text: string) => void,
    onToolCall: (name: string, args: string) => void,
    onToolResult: (name: string, result: string) => void,
    onDone: () => void,
    onError: (err: string) => void,
    maxIterations: number = 8,
  ): Promise<void> {
    // 尝试使用 Rust libzclaw.so (streaming via polling)
    const useNative = false;
    if (useNative && zclawNative && zclawNative.isAvailable()) {
      const ready = await ZClawApi.ensureNative(tokenKey, model);
      if (ready) {
        try {
          const lastUserMsg = [...messages].reverse().find(m => m.role === 'user');
          const userText = lastUserMsg?.content || '';

          const startResult = zclawNative.chat(userText);
          if (startResult !== 0) {
            throw new Error(`zclaw_chat returned ${startResult}`);
          }

          let polling = true;
          while (polling) {
            await new Promise<void>(r => setTimeout(r, 50));
            const chunksJson = zclawNative.pollChunks();
            const chunks: ZClawChunk[] = JSON.parse(chunksJson);

            for (const chunk of chunks) {
              switch (chunk.chunkType) {
                case 0: if (chunk.name) onChunk(chunk.name); break;
                case 1: onToolCall(chunk.name, chunk.args); break;
                case 2: onToolResult(chunk.name, chunk.result); break;
                case 3: polling = false; break;
                case 4: onError(chunk.name); polling = false; break;
                case 5: if (chunk.name) onThinking(chunk.name); break;
              }
            }

            if (polling && !zclawNative.isRunning()) {
              const remainingJson = zclawNative.pollChunks();
              const remaining: ZClawChunk[] = JSON.parse(remainingJson);
              for (const chunk of remaining) {
                switch (chunk.chunkType) {
                  case 0: if (chunk.name) onChunk(chunk.name); break;
                  case 1: onToolCall(chunk.name, chunk.args); break;
                  case 2: onToolResult(chunk.name, chunk.result); break;
                  case 3: polling = false; break;
                  case 4: onError(chunk.name); polling = false; break;
                  case 5: if (chunk.name) onThinking(chunk.name); break;
                }
              }
              if (polling) polling = false;
            }
          }

          onDone();
          return;
        } catch (e) {
          console.warn('Native ZClaw failed, falling back to TS loop: ' + e);
        }
      }
    }

    // ── ArkTS 轻量 agent loop ──
    // 第一轮用流式（SSE 实时输出），后续轮用非流式（工具调用场景）
    const conversationMessages = [...messages];

    // 重置 think 标签过滤状态机
    ZClawApi.thinkParseState = 0;
    ZClawApi.thinkBuffer = '';

    for (let iter = 0; iter < maxIterations; iter++) {
      const isFirstIter = iter === 0;

      try {
        const result = await ZClawApi.callLLM(
          tokenKey, conversationMessages, model, isFirstIter,
          isFirstIter ? { onChunk, onThinking } : undefined,
        );

        // 后续轮非流式：手动回调
        if (!isFirstIter && result.content) {
          onChunk(result.content);
        }

        // 没有 tool_calls → 循环结束
        if (!result.tool_calls || result.tool_calls.length === 0) {
          onDone();
          return;
        }

        // 添加 assistant 消息（含 tool_calls）
        const assistantMsg: AgentMessage = {
          role: 'assistant',
          content: result.content || '',
          tool_calls: result.tool_calls.map((tc: ToolCall): ToolCall => {
            const fn: ToolCallFunction = {
              name: tc.function.name, arguments: tc.function.arguments,
            };
            const toolCall: ToolCall = { id: tc.id, type: 'function', function: fn };
            return toolCall;
          }),
        };
        conversationMessages.push(assistantMsg);

        // 执行每个 tool_call
        for (const tc of result.tool_calls) {
          const toolName = tc.function.name;
          let toolArgs: Record<string, Object> = {};
          try {
            toolArgs = JSON.parse(tc.function.arguments || '{}') as Record<string, Object>;
          } catch {}

          onToolCall(toolName, tc.function.arguments);

          const toolResult = await executeTool(toolName, toolArgs);
          onToolResult(toolName, toolResult);

          const toolMsg: AgentMessage = {
            role: 'tool',
            content: toolResult,
            tool_call_id: tc.id,
          };
          conversationMessages.push(toolMsg);
        }
      } catch (e) {
        onError(String(e));
        return;
      }
    }

    onChunk('\n\n[已达到最大工具调用次数限制]');
    onDone();
  }

  // ===== LLM调用（内部） =====
  // 流式模式：用 httpReq.on('dataReceive') 事件实时接收 SSE chunk
  // 非流式模式：用 httpReq.request() 一次性接收

  static async callLLM(
    tokenKey: string,
    messages: AgentMessage[],
    model: string,
    stream: boolean,
    callbacks?: LLMStreamCallbacks,
  ): Promise<LLMResult> {
    const url = `${Constants.BASE_URL}/v1/chat/completions`;

    const body: Record<string, Object> = {
      'model': model,
      'messages': messages.map((m: AgentMessage): Record<string, Object> => {
        const obj: Record<string, Object> = {};
        obj['role'] = m.role;
        obj['content'] = m.content;
        if (m.tool_calls) obj['tool_calls'] = m.tool_calls;
        if (m.tool_call_id) obj['tool_call_id'] = m.tool_call_id;
        return obj;
      }),
      'stream': stream,
      'tools': getToolDefinitions(),
      'tool_choice': 'auto',
    };

    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
      'Authorization': `Bearer ${tokenKey}`,
    };

    console.info(`ZClawApi.callLLM url=${url} model=${model} stream=${stream} tokenLen=${tokenKey.length}`);

    if (stream && callbacks) {
      // ── 流式模式：用 dataReceive 事件实时接收 SSE ──
      return await ZClawApi.callLLMStream(url, headers, body, callbacks);
    } else {
      // ── 非流式模式 ──
      const httpReq = http.createHttp();
      try {
        const bodyStr = JSON.stringify(body);
        // 非流式不需要 tools，减少 token 消耗
        const nonStreamBody: Record<string, Object> = {
          'model': model,
          'messages': messages.map((m: AgentMessage): Record<string, Object> => {
            const obj: Record<string, Object> = {};
            obj['role'] = m.role;
            obj['content'] = m.content;
            if (m.tool_calls) obj['tool_calls'] = m.tool_calls;
            if (m.tool_call_id) obj['tool_call_id'] = m.tool_call_id;
            return obj;
          }),
          'stream': false,
          'tools': getToolDefinitions(),
          'tool_choice': 'auto',
        };

        const response = await httpReq.request(url, {
          method: http.RequestMethod.POST,
          header: headers,
          extraData: JSON.stringify(nonStreamBody),
          connectTimeout: 30000,
          readTimeout: 120000,
          expectDataType: http.HttpDataType.STRING,
        });

        if (response.responseCode !== 200) {
          const errText = response.result as string;
          console.error(`ZClawApi.callLLM ${response.responseCode}: ${errText.substring(0, 300)}`);
          let errDetail: string = `LLM调用失败: ${response.responseCode}`;
          try {
            const errParsed = JSON.parse(errText) as Record<string, Object>;
            if (errParsed['message']) {
              errDetail = `${errParsed['message']}`;
            } else if (errParsed['error']) {
              const errObj = errParsed['error'] as Record<string, string>;
              errDetail = `${errObj['message'] ?? errText.substring(0, 100)}`;
            }
          } catch {}
          throw new Error(errDetail);
        }

        const text = response.result as string;
        const data = JSON.parse(text) as NonStreamChunk;
        const choice = data.choices?.[0];
        const msg = choice?.message;
        return {
          content: msg?.content || '',
          tool_calls: msg?.tool_calls,
        };
      } finally {
        httpReq.destroy();
      }
    }
  }

  // ── SSE 流式调用 ──
  // HarmonyOS http 模块的 dataReceive 事件可以实时收到 chunk
  // 需要用 expectDataType: STRING + on('dataReceive') 事件
  static async callLLMStream(
    url: string,
    headers: Record<string, string>,
    body: Record<string, Object>,
    callbacks: LLMStreamCallbacks,
  ): Promise<LLMResult> {
    return new Promise<LLMResult>((resolve, reject) => {
      const httpReq = http.createHttp();
      let content = '';
      let toolCalls: ToolCall[] = [];
      let sseBuffer = '';
      let settled = false;

      const finish = (result: LLMResult) => {
        if (settled) return;
        settled = true;
        httpReq.destroy();
        resolve(result);
      };

      const fail = (err: string) => {
        if (settled) return;
        settled = true;
        httpReq.destroy();
        reject(new Error(err));
      };

      // 解析 SSE buffer
      const processSSEBuffer = () => {
        const lines = sseBuffer.split('\n');
        // 保留最后一个可能不完整的行
        sseBuffer = lines.pop() || '';

        for (const line of lines) {
          if (!line.startsWith('data: ')) continue;
          const data = line.slice(6).trim();
          if (data === '[DONE]') {
            finish({ content, tool_calls: toolCalls.length > 0 ? toolCalls : undefined });
            return;
          }

          try {
            const parsed = JSON.parse(data) as StreamChunk;
            const choice = parsed.choices?.[0];
            const delta = choice?.delta;
            if (!delta) continue;

            if (delta.content) {
              // 过滤 <think>...</think> 标签 — 某些模型把思考内容放在 content 里
              const rawContent = delta.content;
              const filtered = ZClawApi.filterThinkTags(rawContent);
              if (filtered.thinking) {
                callbacks.onThinking(filtered.thinking);
              }
              if (filtered.content) {
                content += filtered.content;
                callbacks.onChunk(filtered.content);
              }
            }
            if (delta.reasoning_content) {
              callbacks.onThinking(delta.reasoning_content);
            }
            if (delta.tool_calls) {
              for (const tc of delta.tool_calls) {
                const idx = tc.index ?? 0;
                if (!toolCalls[idx]) {
                  const fn: ToolCallFunction = { name: '', arguments: '' };
                  toolCalls[idx] = { id: tc.id || '', type: 'function', function: fn };
                }
                if (tc.id) toolCalls[idx].id = tc.id;
                if (tc.function?.name) toolCalls[idx].function.name += tc.function.name;
                if (tc.function?.arguments) toolCalls[idx].function.arguments += tc.function.arguments;
              }
            }
          } catch {}
        }
      };

      // dataReceive 事件 — 每收到一段数据就触发
      httpReq.on('dataReceive', (data: ArrayBuffer) => {
        const text = ZClawApi.decodeUtf8(data);
        sseBuffer += text;
        processSSEBuffer();
      });

      // dataReceiveProgress — 有些版本通过此事件给出接收进度
      httpReq.on('dataReceiveProgress', (progress: http.DataReceiveProgressInfo) => {
        // 部分 HarmonyOS 版本在 progress 中携带数据
      });

      const bodyStr = JSON.stringify(body);
      console.info(`ZClawApi.callLLMStream starting, bodyLen=${bodyStr.length}`);

      httpReq.request(url, {
        method: http.RequestMethod.POST,
        header: headers,
        extraData: bodyStr,
        connectTimeout: 30000,
        readTimeout: 120000,
        expectDataType: http.HttpDataType.STRING,
      }).then((response: http.HttpResponse) => {
        // request 完成 — dataReceive 已经分段处理了 SSE
        // 这里做兜底：如果 dataReceive 没触发（某些版本），
        // 从 response.result 一次性解析
        if (!settled) {
          const text = response.result as string;
          if (text && text.length > 0) {
            sseBuffer += text;
            processSSEBuffer();
          }
          if (!settled) {
            finish({ content, tool_calls: toolCalls.length > 0 ? toolCalls : undefined });
          }
        }
      }).catch((e: Error) => {
        const errMsg = e instanceof Error ? e.message : String(e);
        console.error(`ZClawApi.callLLMStream error: ${errMsg}`);
        if (content.length > 0) {
          finish({ content, tool_calls: toolCalls.length > 0 ? toolCalls : undefined });
        } else {
          fail(`流式请求失败: ${errMsg}`);
        }
      });
    });
  }

  // think 标签过滤结果
  static thinkParseState: number = 0;  // 0=normal, 1=in think
  static thinkBuffer: string = '';

  static filterThinkTags(chunk: string): ThinkFilterResult {
    let content = '';
    let thinking = '';

    // 简单状态机：检测 <think> 和 </think> 标签
    // 跨 chunk 的标签可能被截断，用 buffer 处理
    ZClawApi.thinkBuffer += chunk;

    while (ZClawApi.thinkBuffer.length > 0) {
      if (ZClawApi.thinkParseState === 0) {
        // 正常模式：找 <think> 开始
        const openIdx = ZClawApi.thinkBuffer.indexOf('<think>');
        if (openIdx >= 0) {
          // openIdx 之前的是正常内容
          if (openIdx > 0) {
            content += ZClawApi.thinkBuffer.substring(0, openIdx);
          }
          ZClawApi.thinkBuffer = ZClawApi.thinkBuffer.substring(openIdx + 7);
          ZClawApi.thinkParseState = 1;
        } else {
          // 可能是截断的 <think> 前缀
          // 找最后的 < 字符
          const lastLt = ZClawApi.thinkBuffer.lastIndexOf('<');
          if (lastLt >= 0 && lastLt < ZClawApi.thinkBuffer.length) {
            const suffix = ZClawApi.thinkBuffer.substring(lastLt);
            // 检查是否是 <think> 的前缀
            if ('<think>'.startsWith(suffix) && suffix.length < 7) {
              // 可能是截断的标签，输出之前的内容，保留 suffix
              if (lastLt > 0) {
                content += ZClawApi.thinkBuffer.substring(0, lastLt);
              }
              ZClawApi.thinkBuffer = suffix;
              break;
            }
          }
          // 没有标签，全部是正常内容
          content += ZClawApi.thinkBuffer;
          ZClawApi.thinkBuffer = '';
        }
      } else {
        // 思考模式：找 </think> 结束
        const closeIdx = ZClawApi.thinkBuffer.indexOf('</think>');
        if (closeIdx >= 0) {
          thinking += ZClawApi.thinkBuffer.substring(0, closeIdx);
          ZClawApi.thinkBuffer = ZClawApi.thinkBuffer.substring(closeIdx + 8);
          ZClawApi.thinkParseState = 0;
        } else {
          // 可能是截断的 </think> 前缀
          const lastLt = ZClawApi.thinkBuffer.lastIndexOf('<');
          if (lastLt >= 0) {
            const suffix = ZClawApi.thinkBuffer.substring(lastLt);
            if ('</think>'.startsWith(suffix) && suffix.length < 8) {
              thinking += ZClawApi.thinkBuffer.substring(0, lastLt);
              ZClawApi.thinkBuffer = suffix;
              break;
            }
          }
          // 全部是思考内容
          thinking += ZClawApi.thinkBuffer;
          ZClawApi.thinkBuffer = '';
        }
      }
    }

    const result: ThinkFilterResult = {
      content: content,
      thinking: thinking,
    };
    return result;
  }

  // UTF-8 ArrayBuffer → string 解码（鸿蒙无 TextDecoder，手动实现）
  static decodeUtf8(data: ArrayBuffer): string {
    const bytes = new Uint8Array(data);
    let result = '';
    let i = 0;
    while (i < bytes.length) {
      const b1 = bytes[i];
      if (b1 < 0x80) {
        // ASCII
        result += String.fromCharCode(b1);
        i++;
      } else if (b1 < 0xC0) {
        // 无效的续接字节，跳过
        i++;
      } else if (b1 < 0xE0) {
        // 2字节 UTF-8
        if (i + 1 < bytes.length) {
          const b2 = bytes[i + 1];
          const code = ((b1 & 0x1F) << 6) | (b2 & 0x3F);
          result += String.fromCharCode(code);
          i += 2;
        } else {
          i++;
        }
      } else if (b1 < 0xF0) {
        // 3字节 UTF-8（中文常见）
        if (i + 2 < bytes.length) {
          const b2 = bytes[i + 1];
          const b3 = bytes[i + 2];
          const code = ((b1 & 0x0F) << 12) | ((b2 & 0x3F) << 6) | (b3 & 0x3F);
          result += String.fromCharCode(code);
          i += 3;
        } else {
          i++;
        }
      } else if (b1 < 0xF8) {
        // 4字节 UTF-8（emoji 等）
        if (i + 3 < bytes.length) {
          const b2 = bytes[i + 1];
          const b3 = bytes[i + 2];
          const b4 = bytes[i + 3];
          const code = ((b1 & 0x07) << 18) | ((b2 & 0x3F) << 12) | ((b3 & 0x3F) << 6) | (b4 & 0x3F);
          // 转为 UTF-16 代理对
          if (code >= 0x10000) {
            const offset = code - 0x10000;
            const high = 0xD800 + (offset >> 10);
            const low = 0xDC00 + (offset & 0x3FF);
            result += String.fromCharCode(high);
            result += String.fromCharCode(low);
          } else {
            result += String.fromCharCode(code);
          }
          i += 4;
        } else {
          i++;
        }
      } else {
        i++;
      }
    }
    return result;
  }
}
