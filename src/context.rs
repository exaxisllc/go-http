// SPDX-License-Identifier: Apache-2.0

// Re-export go-lib context primitives so callers use `go_http::context` uniformly.
pub use go_lib::context::{background, with_cancel, with_deadline, with_timeout, CancelFn, Context, ContextError};
