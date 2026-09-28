//! The daemon's agent channel (agent-channel contract r6.1): the host's one
//! connection to the daemon for host-bound browser operations, reached
//! through the toolbox's `…/agent/channel` route under the action
//! `ambit_browser_agent`. The channel changes the transport, never the
//! operation: every step is prepared as the spawned MCP client prepares a
//! call (`mcp::host_bound::HostFlags::prepare`) and dispatched through the
//! same host feedback path (`actions::run_host_command`).

pub(crate) mod ceiling;
pub(crate) mod frame;
pub(crate) mod ledger;
