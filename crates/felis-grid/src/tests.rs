use super::*;
use felis_vt::Parser;
use proptest::prelude::*;

use crate::test_support::{drive, responses};

mod bidi_override;
mod clusters;
mod device_replies;
mod fast_path;
mod grid_basics;
mod keyboard_modes;
mod prompt_marks;
mod reflow;
mod reflow_prompt;
mod resize;
mod scroll_directive;
mod soft_wrap;
