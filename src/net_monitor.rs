use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct CodexNetMetrics {
    pub pid_count: usize,
    pub codex_pid_count: usize,
    pub claude_pid_count: usize,
    pub connection_count: usize,
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
    pub per_pid: Vec<PidThroughput>,
    pub sample_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackedTool {
    Codex,
    ClaudeCode,
}

#[derive(Clone, Debug)]
pub struct PidThroughput {
    pub pid: u32,
    pub tool: TrackedTool,
    pub connection_count: usize,
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
}

#[cfg(windows)]
mod imp {
    use super::{CodexNetMetrics, HashMap, HashSet, Instant, PidThroughput, TrackedTool};
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::ptr::{self, null_mut};

    use winapi::ctypes::c_void;
    use winapi::shared::iprtrmib::TCP_TABLE_OWNER_PID_ALL;
    use winapi::shared::minwindef::FALSE;
    use winapi::shared::tcpestats::{
        TCP_ESTATS_DATA_ROD_v0, TCP_ESTATS_DATA_RW_v0, TcpBoolOptEnabled, TcpConnectionEstatsData,
    };
    use winapi::shared::tcpmib::{MIB_TCP_STATE_ESTAB, MIB_TCPROW, MIB_TCPROW_OWNER_PID};
    use winapi::shared::winerror::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
    use winapi::shared::ws2def::AF_INET;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::iphlpapi::{
        GetExtendedTcpTable, GetPerTcpConnectionEStats, SetPerTcpConnectionEStats,
    };
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    #[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
    struct TcpConnectionKey {
        pid: u32,
        local_addr: u32,
        local_port: u32,
        remote_addr: u32,
        remote_port: u32,
    }

    impl TcpConnectionKey {
        fn from_owner_row(row: &MIB_TCPROW_OWNER_PID) -> Self {
            Self {
                pid: row.dwOwningPid,
                local_addr: row.dwLocalAddr,
                local_port: row.dwLocalPort,
                remote_addr: row.dwRemoteAddr,
                remote_port: row.dwRemotePort,
            }
        }
    }

    #[derive(Debug, Clone, Copy, Default)]
    struct TcpByteCounters {
        in_bytes: u64,
        out_bytes: u64,
    }

    pub struct CodexNetMonitor {
        last_at: Option<Instant>,
        last_totals: HashMap<TcpConnectionKey, TcpByteCounters>,
        estats_enabled: HashSet<TcpConnectionKey>,
    }

    impl CodexNetMonitor {
        pub fn new() -> Self {
            Self {
                last_at: None,
                last_totals: HashMap::new(),
                estats_enabled: HashSet::new(),
            }
        }

        pub fn sample(&mut self) -> CodexNetMetrics {
            let now = Instant::now();
            let elapsed_secs = self
                .last_at
                .map(|last| now.duration_since(last).as_secs_f64())
                .unwrap_or_default();
            self.last_at = Some(now);

            let mut first_error = None;
            let tracked_pids = match find_tracked_pids() {
                Ok(pids) => pids,
                Err(err) => {
                    first_error = Some(format!("process scan failed: {err}"));
                    HashMap::new()
                }
            };
            let codex_pid_count = tracked_pids
                .values()
                .filter(|tool| matches!(tool, TrackedTool::Codex))
                .count();
            let claude_pid_count = tracked_pids
                .values()
                .filter(|tool| matches!(tool, TrackedTool::ClaudeCode))
                .count();

            if tracked_pids.is_empty() {
                self.last_totals.clear();
                self.estats_enabled.clear();
                return CodexNetMetrics {
                    pid_count: 0,
                    codex_pid_count: 0,
                    claude_pid_count: 0,
                    connection_count: 0,
                    rx_bytes_per_sec: 0.0,
                    tx_bytes_per_sec: 0.0,
                    per_pid: Vec::new(),
                    sample_error: first_error,
                };
            }

            let rows = match tcp_owner_pid_rows_ipv4() {
                Ok(rows) => rows,
                Err(err) => {
                    return CodexNetMetrics {
                        pid_count: tracked_pids.len(),
                        codex_pid_count,
                        claude_pid_count,
                        connection_count: 0,
                        rx_bytes_per_sec: 0.0,
                        tx_bytes_per_sec: 0.0,
                        per_pid: Vec::new(),
                        sample_error: Some(format!("tcp table failed: {err}")),
                    };
                }
            };

            let mut current = HashMap::new();
            let mut per_pid_connections = HashMap::<u32, usize>::new();
            let mut connection_count = 0usize;
            for row in &rows {
                if !tracked_pids.contains_key(&row.dwOwningPid) {
                    continue;
                }
                if row.dwState != MIB_TCP_STATE_ESTAB {
                    continue;
                }

                let key = TcpConnectionKey::from_owner_row(row);
                let tcp_row = to_tcp_row(row);
                match self.read_connection_totals(key, &tcp_row) {
                    Ok(total) => {
                        connection_count += 1;
                        *per_pid_connections.entry(key.pid).or_insert(0) += 1;
                        current.insert(key, total);
                    }
                    Err(code) => {
                        if first_error.is_none() {
                            first_error = Some(format!("eStats unavailable (code {code})"));
                        }
                    }
                }
            }

            let mut rx_delta = 0u64;
            let mut tx_delta = 0u64;
            let mut per_pid_deltas = HashMap::<u32, TcpByteCounters>::new();
            if elapsed_secs > 0.0 {
                for (key, current_total) in &current {
                    if let Some(previous_total) = self.last_totals.get(key) {
                        let in_delta = current_total
                            .in_bytes
                            .saturating_sub(previous_total.in_bytes);
                        let out_delta = current_total
                            .out_bytes
                            .saturating_sub(previous_total.out_bytes);
                        rx_delta = rx_delta.saturating_add(in_delta);
                        tx_delta = tx_delta.saturating_add(out_delta);

                        let pid_delta = per_pid_deltas.entry(key.pid).or_default();
                        pid_delta.in_bytes = pid_delta.in_bytes.saturating_add(in_delta);
                        pid_delta.out_bytes = pid_delta.out_bytes.saturating_add(out_delta);
                    }
                }
            }

            self.last_totals = current;
            let mut per_pid = per_pid_connections
                .into_iter()
                .filter_map(|(pid, pid_conn_count)| {
                    let tool = *tracked_pids.get(&pid)?;
                    let totals = per_pid_deltas.get(&pid).copied().unwrap_or_default();
                    Some(PidThroughput {
                        pid,
                        tool,
                        connection_count: pid_conn_count,
                        rx_bytes_per_sec: (totals.in_bytes as f64) / elapsed_secs.max(1e-6),
                        tx_bytes_per_sec: (totals.out_bytes as f64) / elapsed_secs.max(1e-6),
                    })
                })
                .collect::<Vec<_>>();
            per_pid.sort_by(|left, right| {
                let left_total = left.rx_bytes_per_sec + left.tx_bytes_per_sec;
                let right_total = right.rx_bytes_per_sec + right.tx_bytes_per_sec;
                right_total
                    .total_cmp(&left_total)
                    .then_with(|| left.pid.cmp(&right.pid))
            });

            CodexNetMetrics {
                pid_count: tracked_pids.len(),
                codex_pid_count,
                claude_pid_count,
                connection_count,
                rx_bytes_per_sec: (rx_delta as f64) / elapsed_secs.max(1e-6),
                tx_bytes_per_sec: (tx_delta as f64) / elapsed_secs.max(1e-6),
                per_pid,
                sample_error: first_error,
            }
        }

        fn read_connection_totals(
            &mut self,
            key: TcpConnectionKey,
            row: &MIB_TCPROW,
        ) -> Result<TcpByteCounters, u32> {
            if !self.estats_enabled.contains(&key) {
                enable_data_estats_collection(row)?;
                self.estats_enabled.insert(key);
            }
            read_data_estats(row)
        }
    }

    fn find_tracked_pids() -> io::Result<HashMap<u32, TrackedTool>> {
        // SAFETY: snapshot handle is checked and closed; PROCESSENTRY32W is initialized per API contract.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }

            let mut pids = HashMap::new();
            let mut entry: PROCESSENTRY32W = zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    let process_name = utf16_name(&entry.szExeFile);
                    if let Some(tool) = classify_tracked_tool(&process_name) {
                        pids.insert(entry.th32ProcessID, tool);
                    }

                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }

            CloseHandle(snapshot);
            Ok(pids)
        }
    }

    fn classify_tracked_tool(process_name: &str) -> Option<TrackedTool> {
        let name = process_name.to_ascii_lowercase();
        if name.contains("codex") {
            return Some(TrackedTool::Codex);
        }

        if name == "claude.exe"
            || name == "claude"
            || name.contains("claude-code")
            || name.contains("claude_code")
        {
            return Some(TrackedTool::ClaudeCode);
        }

        None
    }

    fn utf16_name(raw_name: &[u16]) -> String {
        let end = raw_name
            .iter()
            .position(|ch| *ch == 0)
            .unwrap_or(raw_name.len());
        String::from_utf16_lossy(&raw_name[..end])
    }

    fn tcp_owner_pid_rows_ipv4() -> io::Result<Vec<MIB_TCPROW_OWNER_PID>> {
        // SAFETY: API is called with valid pointers and buffer length is obtained from the first probe call.
        unsafe {
            let mut bytes_needed = 0u32;
            let probe = GetExtendedTcpTable(
                ptr::null_mut(),
                &mut bytes_needed,
                FALSE,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );

            if probe != ERROR_INSUFFICIENT_BUFFER {
                return Err(io::Error::from_raw_os_error(probe as i32));
            }

            let mut buffer_u32 = vec![0u32; bytes_needed.div_ceil(4) as usize];
            let status = GetExtendedTcpTable(
                buffer_u32.as_mut_ptr() as *mut c_void,
                &mut bytes_needed,
                FALSE,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if status != NO_ERROR {
                return Err(io::Error::from_raw_os_error(status as i32));
            }

            let base = buffer_u32.as_ptr() as *const u8;
            let row_count = *(base as *const u32) as usize;
            let rows_ptr = base.add(size_of::<u32>()) as *const MIB_TCPROW_OWNER_PID;
            let rows = std::slice::from_raw_parts(rows_ptr, row_count);
            Ok(rows.to_vec())
        }
    }

    fn to_tcp_row(owner_row: &MIB_TCPROW_OWNER_PID) -> MIB_TCPROW {
        MIB_TCPROW {
            State: owner_row.dwState as _,
            dwLocalAddr: owner_row.dwLocalAddr,
            dwLocalPort: owner_row.dwLocalPort,
            dwRemoteAddr: owner_row.dwRemoteAddr,
            dwRemotePort: owner_row.dwRemotePort,
        }
    }

    fn enable_data_estats_collection(row: &MIB_TCPROW) -> Result<(), u32> {
        // SAFETY: row points to a valid TCP row and RW struct points to initialized memory.
        unsafe {
            let mut rw = TCP_ESTATS_DATA_RW_v0 {
                EnableCollection: TcpBoolOptEnabled as u8,
            };
            let status = SetPerTcpConnectionEStats(
                row as *const MIB_TCPROW as *mut MIB_TCPROW,
                TcpConnectionEstatsData,
                (&mut rw as *mut TCP_ESTATS_DATA_RW_v0).cast(),
                0,
                size_of::<TCP_ESTATS_DATA_RW_v0>() as u32,
                0,
            );
            if status == NO_ERROR {
                Ok(())
            } else {
                Err(status)
            }
        }
    }

    fn read_data_estats(row: &MIB_TCPROW) -> Result<TcpByteCounters, u32> {
        // SAFETY: row points to a valid TCP row and ROD struct is correctly sized for API output.
        unsafe {
            let mut rod: TCP_ESTATS_DATA_ROD_v0 = zeroed();
            let status = GetPerTcpConnectionEStats(
                row as *const MIB_TCPROW as *mut MIB_TCPROW,
                TcpConnectionEstatsData,
                null_mut(),
                0,
                0,
                null_mut(),
                0,
                0,
                (&mut rod as *mut TCP_ESTATS_DATA_ROD_v0).cast(),
                0,
                size_of::<TCP_ESTATS_DATA_ROD_v0>() as u32,
            );

            if status == NO_ERROR {
                Ok(TcpByteCounters {
                    in_bytes: rod.DataBytesIn,
                    out_bytes: rod.DataBytesOut,
                })
            } else {
                Err(status)
            }
        }
    }
}

#[cfg(windows)]
pub use imp::CodexNetMonitor;

#[cfg(not(windows))]
pub struct CodexNetMonitor;

#[cfg(not(windows))]
impl CodexNetMonitor {
    pub fn new() -> Self {
        Self
    }

    pub fn sample(&mut self) -> CodexNetMetrics {
        CodexNetMetrics {
            pid_count: 0,
            codex_pid_count: 0,
            claude_pid_count: 0,
            connection_count: 0,
            rx_bytes_per_sec: 0.0,
            tx_bytes_per_sec: 0.0,
            per_pid: Vec::new(),
            sample_error: Some(String::from(
                "network monitor is only implemented on Windows",
            )),
        }
    }
}
