use std::{error::Error, fmt};

/// A named-view mount failure, available by downcasting the returned `anyhow::Error`.
///
/// ```
/// # use gpui_shell::MountViewError;
/// fn generation_was_released(error: &anyhow::Error) -> bool {
///     error.downcast_ref::<MountViewError>()
///         .is_some_and(MountViewError::is_generation_released)
/// }
/// ```
#[derive(Debug)]
pub struct MountViewError {
    source: anyhow::Error,
    generation_released: bool,
}

impl MountViewError {
    pub(crate) fn before_construction(source: anyhow::Error) -> anyhow::Error {
        Self {
            source,
            generation_released: false,
        }
        .into()
    }

    pub(crate) fn after_construction(source: anyhow::Error) -> anyhow::Error {
        Self {
            source,
            generation_released: true,
        }
        .into()
    }

    /// Whether this attempt released the application's generation.
    ///
    /// Constructor and initialization failures release it. Failures before
    /// construction leave it unchanged, including owner, policy and export checks.
    pub fn is_generation_released(&self) -> bool {
        self.generation_released
    }
}

impl fmt::Display for MountViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.source, f)
    }
}

impl Error for MountViewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}
