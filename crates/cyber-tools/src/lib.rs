//! cyber-tools: 网络安全专项工具封装。
//!
//! 提供针对渗透测试、Web 安全、密码学、二进制分析的核心原生工具集。

pub mod binary_inspect;
pub mod cyberchef;
pub mod dns_recon;
pub mod fuzz_endpoint;
pub mod hash_identifier;
pub mod http_request;
pub mod jwt_analyzer;
pub mod poc_validator;
pub mod port_scanner;
pub mod reverse_shell_gen;

pub use binary_inspect::BinaryInspectTool;
pub use cyberchef::CyberChefTool;
pub use dns_recon::DnsReconTool;
pub use fuzz_endpoint::FuzzEndpointTool;
pub use hash_identifier::HashIdentifierTool;
pub use http_request::HttpRequestTool;
pub use jwt_analyzer::JwtAnalyzerTool;
pub use poc_validator::PocValidatorTool;
pub use port_scanner::PortScannerTool;
pub use reverse_shell_gen::ReverseShellGenTool;

use cyber_agent::ToolRegistry;

/// 向统一工具表注入所有 cyber-tools 内置安全工具。
pub fn register_security_tools(reg: &mut ToolRegistry) {
    reg.register(Box::new(HttpRequestTool));
    reg.register(Box::new(CyberChefTool::new()));
    reg.register(Box::new(HashIdentifierTool));
    reg.register(Box::new(PortScannerTool));
    reg.register(Box::new(DnsReconTool));
    reg.register(Box::new(FuzzEndpointTool));
    reg.register(Box::new(JwtAnalyzerTool));
    reg.register(Box::new(BinaryInspectTool));
    reg.register(Box::new(PocValidatorTool));
    reg.register(Box::new(ReverseShellGenTool));
}
