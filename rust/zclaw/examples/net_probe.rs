//! 验证 network_probe 分层诊断输出。用真实服务器 + 一个坏域名对照。
use zclaw::providers::compatible::network_probe;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().worker_threads(2).build().unwrap();

    let key = std::env::var("ZCLAW_PROBE_KEY").unwrap_or_else(|_| "sk-invalid-for-probe".into());
    println!("key len={}", key.len());

    println!("\n===== ai.ulnit.com（真实服务器，key 可能无效——只关心 TLS/通路）=====");
    for line in rt.block_on(network_probe("https://ai.ulnit.com/v1", &key)) {
        println!("  {}", line);
    }

    println!("\n===== 坏域名对照（应报 DNS 失败）=====");
    for line in rt.block_on(network_probe("https://no-such-host-9x7q.invalid/v1", &key)) {
        println!("  {}", line);
    }

    println!("\n===== 自签证书对照（应报 TLS/证书错误）=====");
    for line in rt.block_on(network_probe("https://self-signed.badssl.com/v1", &key)) {
        println!("  {}", line);
    }
}
