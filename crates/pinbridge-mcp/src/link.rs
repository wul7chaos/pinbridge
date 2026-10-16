//! `pinbridge_client::AgentClient` → `pinbridge_hub::TargetLink` 的适配器。
//!
//! ## 为什么适配器放在 MCP 这一层
//!
//! `pinbridge-hub` 只声明它需要什么（`TargetLink`），`pinbridge-client` 只负责
//! "怎么跟 agent 通话"。把两者接在一起是**组合根**的职责，而 MCP 服务正是这个
//! 组合根：它同时依赖 Hub 与 client，且是唯一知道"这次会话连的是哪个目标"的地方。
//!
//! 放在 Hub 里会让 Hub 依赖 socket 层；放在 client 里会让 client 反向依赖 Hub。
//! 两个方向都是错的——所以它在这里。
//!
//! ## 长度检查在两处都做
//!
//! 适配器检查一次（给出"agent 回了 N 字节、我要 M 字节"这种链路层信息），
//! Hub 再检查一次（那是语义层的最终把关）。重复不是冗余：两处的错误信息
//! 面向的排查方向不同。

use pinbridge_client::{AgentClient, Transport};
use pinbridge_core::error::{Error, Result};
use pinbridge_hub::TargetLink;
use pinbridge_proto::agent_link::{AgentRequest, AgentResponse, ModuleInfo};

/// 把 `AgentClient` 包装成 Hub 能用的链路。
#[derive(Debug)]
pub struct ClientLink<T: Transport> {
    client: AgentClient<T>,
}

impl<T: Transport> ClientLink<T> {
    /// 用一条已建立的 agent 链路创建。
    #[must_use]
    pub fn new(client: AgentClient<T>) -> Self {
        Self { client }
    }

    /// 取回内部 client（例如给别的用途复用同一连接）。
    pub fn client_mut(&mut self) -> &mut AgentClient<T> {
        &mut self.client
    }
}

/// 解析 `ReadMemory` 的应答。
///
/// 单独抽出来是为了能被单测直接覆盖：这一段是**整条链路的语义收缩点**
/// （hex ↔ 字节、错误码 ↔ `Error`），出错时表现为"读到的内存不对"而不是崩溃，
/// 属于最难查的一类问题。
fn decode_memory_response(requested: u32, response: &AgentResponse) -> Result<Vec<u8>> {
    match response {
        AgentResponse::Memory { hex_bytes, .. } => {
            let bytes = decode_hex(hex_bytes)?;
            if bytes.len() != requested as usize {
                return Err(Error::Protocol(format!(
                    "agent 对 read_memory 回了 {} 字节，请求的是 {requested} 字节\
                     （长度不符必须报错：把半截数据当完整数据用会让结论建立在假前提上）",
                    bytes.len()
                )));
            }
            Ok(bytes)
        }
        AgentResponse::Error { code, message } => Err(Error::TargetUnavailable(format!(
            "agent 拒绝读内存（{code}）：{message}"
        ))),
        other => Err(Error::Protocol(format!(
            "agent 对 read_memory 回了意外类型：{other:?}"
        ))),
    }
}

/// 解析 hex 字符串。
///
/// 奇数长度与非法字符都明确报错：这两者在链路上意味着**对端实现有问题**，
/// 而不是"数据大概是这些"。
fn decode_hex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(Error::Protocol(format!(
            "agent 返回的 hex 长度为奇数（{} 个字符），无法解析",
            text.len()
        )));
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    // `as_chunks` 而不是 `chunks_exact(2)`：长度已在上一步校验为偶数，
    // 这里要的是"每两个字节一组"这一静态事实，而不是运行时的过滤器。
    for pair in bytes.as_chunks::<2>().0 {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(Error::Protocol(format!(
            "agent 返回的 hex 里有非法字符 {:?}",
            char::from(other)
        ))),
    }
}

impl<T: Transport> TargetLink for ClientLink<T> {
    fn read_memory(&mut self, address: u64, length: u32) -> Result<Vec<u8>> {
        let response = self
            .client
            .call(&AgentRequest::ReadMemory { address, length })
            .map_err(|error| Error::TargetUnavailable(format!("读内存请求失败：{error}")))?;
        decode_memory_response(length, &response)
    }

    fn list_modules(&mut self) -> Result<(Vec<ModuleInfo>, u32)> {
        let response = self
            .client
            .call(&AgentRequest::Modules {})
            .map_err(|error| Error::TargetUnavailable(format!("列模块请求失败：{error}")))?;
        match response {
            AgentResponse::Modules { modules, skipped } => Ok((modules, skipped)),
            AgentResponse::Error { code, message } => Err(Error::TargetUnavailable(format!(
                "agent 拒绝列模块（{code}）：{message}"
            ))),
            other => Err(Error::Protocol(format!(
                "agent 对 modules 回了意外类型：{other:?}"
            ))),
        }
    }
    fn describe(&self) -> String {
        format!("agent 链路（{}）", self.client.transport_description())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinbridge_client::MemoryTransport;
    use pinbridge_core::capability::Capability;
    use pinbridge_proto::agent_link::AgentResponse;

    /// 用**真实类型**构造应答再序列化，而不是手写 JSON 字面量。
    ///
    /// 手写一个 `{"Memory": {...}}` 看起来等价，实际会和线上格式脱节：
    /// 线上信封是 `{"res": "memory", ...}`（`#[serde(tag = "res")]`）。
    /// 第一版就是这么写的，结果每条测试都在"解析 agent 响应失败"上失败——
    /// 测的是我的想象，不是真实格式。
    fn link_with(response: AgentResponse) -> ClientLink<MemoryTransport> {
        let transport = MemoryTransport::new([serde_json::to_vec(&response).unwrap()]);
        // 注意这个能力集：`AgentClient` **自己也有一道闸门**（默认空集），
        // 不给它 `mem_read`，每个请求都会在客户端就被拒成
        // "能力不足: 需要 MemRead，当前持有 CapabilitySet(0)"。
        // 于是整条链路上其实有**三重**闸门：client → hub → agent 侧导出面。
        ClientLink::new(
            AgentClient::new(transport).with_capabilities(Capability::read_only_baseline()),
        )
    }

    fn memory(address: u64, hex_bytes: &str) -> AgentResponse {
        AgentResponse::Memory {
            address,
            hex_bytes: hex_bytes.to_owned(),
        }
    }

    #[test]
    fn read_memory_decodes_hex_and_checks_length() {
        let mut link = link_with(memory(0x1000, "90cc4889e5"));
        let bytes = link.read_memory(0x1000, 5).expect("长度相符应当成功");
        assert_eq!(bytes, vec![0x90, 0xcc, 0x48, 0x89, 0xe5]);
    }

    #[test]
    fn length_mismatch_is_an_error_not_a_truncation() {
        // agent 少回了一个字节：这是"半截数据"，必须报错。
        let mut link = link_with(memory(0x1000, "90cc"));
        let error = link.read_memory(0x1000, 3).expect_err("长度不符必须失败");
        let text = error.to_string();
        assert!(
            text.contains("回了 2 字节"),
            "错误信息要给出实际长度：{text}"
        );
        assert!(
            text.contains("请求的是 3 字节"),
            "错误信息要给出请求长度：{text}"
        );
    }

    #[test]
    fn agent_error_becomes_target_unavailable_with_its_reason() {
        let mut link = link_with(AgentResponse::Error {
            code: "not_supported".into(),
            message: "read_memory: 单次读取上限 65536 字节".into(),
        });
        let error = link
            .read_memory(0x1000, 8)
            .expect_err("agent 报错时不该成功");
        let text = error.to_string();
        assert!(
            text.contains("单次读取上限"),
            "agent 的原因要原样带出来：{text}"
        );
    }

    #[test]
    fn unexpected_response_type_is_a_protocol_error() {
        let mut link = link_with(AgentResponse::Ok);
        let error = link.read_memory(0x1000, 1).expect_err("类型不符必须失败");
        assert!(
            matches!(error, Error::Protocol(_)),
            "对端回错类型是协议问题，不是目标不可用，实际: {error:?}"
        );
    }

    #[test]
    fn odd_length_hex_is_rejected() {
        let mut link = link_with(memory(0, "abc"));
        let error = link.read_memory(0, 2).expect_err("奇数长度 hex 必须被拒");
        assert!(error.to_string().contains("奇数"), "实际: {error}");
    }

    #[test]
    fn a_client_without_capabilities_is_refused_before_the_link_is_used() {
        // 记录一个容易踩的坑：空的客户端能力集会让**每个**请求在本地就被拒。
        // 错误信息里带"当前持有 CapabilitySet(0)"，看到它就该想到是适配器
        // 没带上能力集，而不是目标不支持。
        let transport = MemoryTransport::new(Vec::<Vec<u8>>::new());
        let mut link = ClientLink::new(AgentClient::new(transport));
        let error = link.read_memory(0x1000, 1).expect_err("空能力集必须被拒");
        let text = error.to_string();
        assert!(text.contains("MemRead"), "错误信息要点出缺哪个能力：{text}");
        println!("✓ 客户端闸门先拦住：{text}");
    }

    #[test]
    fn list_modules_passes_through_the_skip_count() {
        // skipped 必须原样带出来：它是"这份列表可能不完整"的唯一线索。
        let mut link = link_with(AgentResponse::Modules {
            modules: vec![ModuleInfo {
                name: "TARGET.exe".into(),
                base: 0x1_4000_0000,
                size: 0x2000,
                entry_point: 0x1_4000_1000,
            }],
            skipped: 3,
        });
        let (modules, skipped) = link.list_modules().expect("应当成功");
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].base, 0x1_4000_0000);
        assert_eq!(skipped, 3, "跳过计数不能被吞掉");
    }

    #[test]
    fn list_modules_maps_agent_error_to_target_unavailable() {
        let mut link = link_with(AgentResponse::Error {
            code: "not_supported".into(),
            message: "modules: 当前平台没有 PEB".into(),
        });
        let error = link.list_modules().expect_err("agent 报错时不该成功");
        assert!(
            error.to_string().contains("当前平台没有 PEB"),
            "实际: {error}"
        );
    }

    #[test]
    fn describe_names_the_transport_so_errors_are_locatable() {
        let link = link_with(AgentResponse::Ok);
        let text = link.describe();
        assert!(!text.is_empty(), "链路描述不能为空（错误信息要用它）");
        println!("链路描述：{text}");
    }
}
