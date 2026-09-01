// api/ZClawTools.ets — ZClaw 内置工具集（鸿蒙端轻量agent循环）
import http from '@ohos.net.http';
import { Constants } from '../common/Constants';
import { Utils } from '../utils/Utils';

// ── OpenAI function calling 格式的工具定义 ──
export interface ToolParamProperty {
  type: string;
  description: string;
}

export interface ToolParameters {
  type: string;
  properties: Record<string, ToolParamProperty>;
  required: string[];
}

export interface ToolFunctionDef {
  name: string;
  description: string;
  parameters: ToolParameters;
}

export interface ToolDefinition {
  type: string;
  function: ToolFunctionDef;
}

// ── DuckDuckGo API 响应接口 ──
interface DuckDuckGoTopic {
  Text?: string;
}

interface DuckDuckGoResponse {
  AbstractText?: string;
  Answer?: string;
  RelatedTopics?: DuckDuckGoTopic[];
}

// ── Wttr.in 天气API 响应接口 ──
interface WttrDesc {
  value?: string;
}

interface WttrCondition {
  temp_C?: string;
  FeelsLikeC?: string;
  humidity?: string;
  windspeedKmph?: string;
  weatherDesc?: WttrDesc[];
}

interface WttrResponse {
  current_condition?: WttrCondition[];
}

// ════════════════════════════════════════
//  工具定义（发给LLM的function schema）
// ════════════════════════════════════════

export function getToolDefinitions(): ToolDefinition[] {
  // web_search
  const wsProp: ToolParamProperty = { type: 'string', description: '搜索关键词' };
  const wsParams: ToolParameters = {
    type: 'object',
    properties: { 'query': wsProp },
    required: ['query'],
  };
  const wsFunc: ToolFunctionDef = {
    name: 'web_search',
    description: '搜索互联网获取最新信息。当用户询问你不确定的事实、新闻、天气、实时数据时使用。',
    parameters: wsParams,
  };
  const wsTool: ToolDefinition = { type: 'function', function: wsFunc };

  // http_get
  const hgProp: ToolParamProperty = { type: 'string', description: '要请求的URL' };
  const hgParams: ToolParameters = {
    type: 'object',
    properties: { 'url': hgProp },
    required: ['url'],
  };
  const hgFunc: ToolFunctionDef = {
    name: 'http_get',
    description: '发起HTTP GET请求获取指定URL的内容。用于读取网页、调用API。',
    parameters: hgParams,
  };
  const hgTool: ToolDefinition = { type: 'function', function: hgFunc };

  // calculator
  const calcProp: ToolParamProperty = { type: 'string', description: '数学表达式' };
  const calcParams: ToolParameters = {
    type: 'object',
    properties: { 'expression': calcProp },
    required: ['expression'],
  };
  const calcFunc: ToolFunctionDef = {
    name: 'calculator',
    description: '数学计算器。支持加减乘除、幂运算、三角函数等。输入合法的数学表达式。',
    parameters: calcParams,
  };
  const calcTool: ToolDefinition = { type: 'function', function: calcFunc };

  // get_datetime
  const dtParams: ToolParameters = {
    type: 'object',
    properties: {},
    required: [],
  };
  const dtFunc: ToolFunctionDef = {
    name: 'get_datetime',
    description: '获取当前日期时间、星期、时区信息。',
    parameters: dtParams,
  };
  const dtTool: ToolDefinition = { type: 'function', function: dtFunc };

  // weather
  const wProp: ToolParamProperty = { type: 'string', description: '城市名称' };
  const wParams: ToolParameters = {
    type: 'object',
    properties: { 'city': wProp },
    required: ['city'],
  };
  const wFunc: ToolFunctionDef = {
    name: 'weather',
    description: '查询指定城市的天气。',
    parameters: wParams,
  };
  const wTool: ToolDefinition = { type: 'function', function: wFunc };

  return [wsTool, hgTool, calcTool, dtTool, wTool];
}

// ════════════════════════════════════════
//  工具执行器
// ════════════════════════════════════════

export async function executeTool(name: string, args: Record<string, Object>): Promise<string> {
  try {
    if (name === 'web_search') {
      const query = String(args['query'] || '');
      // DuckDuckGo Instant Answer API（无需key）
      const httpReq = http.createHttp();
      try {
        const response = await httpReq.request(
          `https://api.duckduckgo.com/?q=${encodeURIComponent(query)}&format=json&no_html=1`,
          {
            method: http.RequestMethod.GET,
            connectTimeout: 10000,
            readTimeout: 15000,
            expectDataType: http.HttpDataType.STRING,
          }
        );
        if (response.responseCode === 200) {
          const ddg = JSON.parse(response.result as string) as DuckDuckGoResponse;
          const abstract = String(ddg.AbstractText || ddg.Answer || '');
          const rawTopics = ddg.RelatedTopics || [];
          const related = rawTopics.slice(0, 3)
            .map((t: DuckDuckGoTopic): string => String(t.Text || ''))
            .filter(s => s.length > 0)
            .join('\n');
          return `搜索"${query}":\n${abstract}\n${related}`.slice(0, 2000) || `未找到"${query}"的相关信息`;
        }
        return `搜索失败`;
      } finally {
        httpReq.destroy();
      }
    }

    if (name === 'http_get') {
      const url = String(args['url'] || '');
      const httpReq = http.createHttp();
      try {
        const response = await httpReq.request(url, {
          method: http.RequestMethod.GET,
          connectTimeout: 10000,
          readTimeout: 15000,
          expectDataType: http.HttpDataType.STRING,
        });
        if (response.responseCode === 200) {
          return (response.result as string).slice(0, 2000);
        }
        return `HTTP请求失败: ${response.responseCode}`;
      } finally {
        httpReq.destroy();
      }
    }

    if (name === 'calculator') {
      const expr = String(args['expression'] || '');
      try {
        const result = Utils.safeEval(expr);
        return `${expr} = ${result}`;
      } catch {
        return `计算失败: ${expr}`;
      }
    }

    if (name === 'get_datetime') {
      const now = new Date();
      const tz = Intl.DateTimeFormat().resolvedOptions().timeZone;
      const weekdays: string[] = ['日', '一', '二', '三', '四', '五', '六'];
      const weekday = weekdays[now.getDay()];
      return `当前时间: ${now.getFullYear()}年${now.getMonth() + 1}月${now.getDate()}日 星期${weekday} ${now.getHours()}:${String(now.getMinutes()).padStart(2, '0')}:${String(now.getSeconds()).padStart(2, '0')} 时区: ${tz}`;
    }

    if (name === 'weather') {
      const city = String(args['city'] || '');
      const httpReq = http.createHttp();
      try {
        const response = await httpReq.request(
          `https://wttr.in/${encodeURIComponent(city)}?format=j1`,
          {
            method: http.RequestMethod.GET,
            connectTimeout: 10000,
            readTimeout: 15000,
            expectDataType: http.HttpDataType.STRING,
          }
        );
        if (response.responseCode === 200) {
          const data = JSON.parse(response.result as string) as WttrResponse;
          const current = data.current_condition?.[0];
          if (current) {
            const desc = current.weatherDesc?.[0];
            return `${city}天气: ${String(desc?.value || '未知')} 温度${String(current.temp_C)}°C 体感${String(current.FeelsLikeC)}°C 湿度${String(current.humidity)}% 风速${String(current.windspeedKmph)}km/h`;
          }
        }
        return `无法获取${city}的天气信息`;
      } finally {
        httpReq.destroy();
      }
    }

    return `未知工具: ${name}`;
  } catch (e) {
    const err = e as Error;
    return `工具执行错误: ${err.message}`;
  }
}
