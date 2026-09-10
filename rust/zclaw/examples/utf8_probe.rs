//! 验证 serde_json 对 emoji / 中文的输出字节形态，并模拟 JNI Modified-UTF-8 校验
fn main() {
    // 1. 真实 chunk 形态：含中文 + emoji
    let v = serde_json::json!([
        {"chunkType": 0, "name": "我是 **ZClaw**，一个助手！😊"},
        {"chunkType": 3}
    ]);
    let s = serde_json::to_string(&v).unwrap();
    println!("=== serde_json 输出 ===");
    println!("string len(chars) = {}", s.chars().count());
    println!("string len(bytes) = {}", s.len());
    println!("raw = {}", s);

    // 2. emoji 是否被转义为 \uXXXX？
    println!("\n=== 转义检查 ===");
    println!("contains literal emoji 😊 : {}", s.contains('😊'));
    println!("contains \\u escape       : {}", s.contains("\\u"));
    let emoji_bytes: Vec<u8> = "😊".bytes().collect();
    println!("emoji UTF-8 bytes = {:02X?}", emoji_bytes);
    let f = format!("{:02X?}", s.as_bytes());
    println!("JSON 里是否含裸 F0 9F 98 8A : {}", f.contains("F0 9F 98 8A"));

    // 3. 模拟 Android ART 的 Modified-UTF-8 校验（NewStringUTF 的前置条件）
    //    JNI spec: 补充平面字符必须编码为 6 字节代理对 (ED A0-BF xx ED B0-BF xx)
    //    标准 4 字节 F0..F4 开头序列 => 非法
    fn is_valid_modified_utf8(b: &[u8]) -> Result<(), String> {
        let mut i = 0usize;
        while i < b.len() {
            let c = b[i];
            let need = if c < 0x80 { 0 }
                else if (c & 0xE0) == 0xC0 { 1 }
                else if (c & 0xF0) == 0xE0 { 2 }
                else if (c & 0xF8) == 0xF0 { 3 }
                else { return Err(format!("非法起始字节 {:02X} @{}", c, i)); };
            if i + need >= b.len() && need > 0 {
                return Err(format!("截断的多字节序列 @{}", i));
            }
            if need == 3 {
                return Err(format!(
                    "4 字节序列 F0..F7 @{} —— JNI Modified-UTF-8 禁止（须为 6 字节代理对），\
                     NewStringUTF 行为未定义", i));
            }
            for k in 1..=need {
                if (b[i + k] & 0xC0) != 0x80 {
                    return Err(format!("非法后续字节 @{}", i + k));
                }
            }
            i += need + 1;
        }
        Ok(())
    }

    println!("\n=== NewStringUTF 合法性 ===");
    match is_valid_modified_utf8(s.as_bytes()) {
        Ok(()) => println!("VALID  — 可安全传给 NewStringUTF"),
        Err(e) => println!("INVALID — {}", e),
    }

    // 4. 对比：纯中文（BMP，3 字节）是否安全
    let zh = serde_json::to_string(&serde_json::json!([{"chunkType":0,"name":"你好世界"}])).unwrap();
    match is_valid_modified_utf8(zh.as_bytes()) {
        Ok(()) => println!("纯中文 JSON      : VALID（3 字节 BMP，Modified-UTF-8 与标准一致）"),
        Err(e) => println!("纯中文 JSON      : INVALID {}", e),
    }

    // 5. 若走 \u 转义则安全 —— 验证手动转义方案可行
    let escaped: String = s.chars().map(|c| {
        if (c as u32) > 0x7F { format!("\\u{:04x}", c as u32) } else { c.to_string() }
    }).collect();
    println!("\n=== ASCII 转义后 ===");
    println!("all ascii = {}", escaped.is_ascii());
    match is_valid_modified_utf8(escaped.as_bytes()) {
        Ok(()) => println!("VALID"),
        Err(e) => println!("INVALID {}", e),
    }
}
