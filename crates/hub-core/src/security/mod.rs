//! Secret masking and terminal-safe text: ports of `src/security/mask.ts` and `src/tui/sanitize.ts`.

mod clean;
mod mask;

pub use clean::{clean_input, clean_text};
pub use mask::{looks_secret, mask_secrets};
