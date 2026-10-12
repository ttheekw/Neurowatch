//! Recording subsystem — state management, segment tracking, and retention
//! (camera quotas and the disk threshold).

pub mod mkv_timing;
pub mod quota;
pub mod retention;
pub mod segment_tracker;
pub mod supervisor;
