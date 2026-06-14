//! Subagent tools — oneshot and reactive (start/status/steer/collect/resume).

mod oneshot;
pub mod start;
pub mod status;
pub mod steer;
pub mod collect;
pub mod resume;

pub use oneshot::SubagentTool;
pub use start::SubagentStartTool;
pub use status::SubagentStatusTool;
pub use steer::SubagentSteerTool;
pub use collect::SubagentCollectTool;
pub use resume::SubagentResumeTool;
