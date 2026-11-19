use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Default)]
pub(crate) struct UpdateCancellation<'a> {
    requested: Option<&'a AtomicBool>,
    #[cfg(test)]
    pub(crate) checkpoint: Option<&'a dyn Fn(UpdateCheckpoint<'_>)>,
}

impl<'a> UpdateCancellation<'a> {
    pub(crate) fn for_batch(requested: &'a AtomicBool) -> Self {
        Self {
            requested: Some(requested),
            #[cfg(test)]
            checkpoint: None,
        }
    }

    pub(crate) fn is_requested(self) -> bool {
        self.requested
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    pub(crate) fn before_staging(self) -> io::Result<()> {
        #[cfg(test)]
        if let Some(checkpoint) = self.checkpoint {
            checkpoint(UpdateCheckpoint::BeforeStaging);
        }
        self.check(None)
    }

    pub(crate) fn before_apply(self, staging: &Path) -> io::Result<()> {
        #[cfg(test)]
        if let Some(checkpoint) = self.checkpoint {
            checkpoint(UpdateCheckpoint::BeforeApply(staging));
        }
        self.check(Some(staging))
    }

    fn check(self, staging: Option<&Path>) -> io::Result<()> {
        if self.is_requested() {
            return Err(cancelled(staging));
        }
        Ok(())
    }

    /// Called under the update lock. The atomic read-modify-write is ordered with the
    /// signal handler's store and marks the decision to start writing intent. Once that
    /// decision wins, finish the update or leave it recoverable.
    pub(super) fn record_intent(
        self,
        staging: &Path,
        write: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        #[cfg(test)]
        if let Some(checkpoint) = self.checkpoint {
            checkpoint(UpdateCheckpoint::BeforeIntent(staging));
        }
        if self
            .requested
            .is_some_and(|flag| flag.fetch_or(false, Ordering::SeqCst))
        {
            return Err(cancelled(Some(staging)));
        }
        write()?;
        #[cfg(test)]
        if let Some(checkpoint) = self.checkpoint {
            checkpoint(UpdateCheckpoint::AfterIntent(staging));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) enum UpdateCheckpoint<'a> {
    BeforeStaging,
    BeforeApply(&'a Path),
    BeforeIntent(&'a Path),
    AfterIntent(&'a Path),
}

#[derive(Debug)]
pub(crate) struct UpdateCancelled(Option<PathBuf>);

impl UpdateCancelled {
    pub(crate) fn is(error: &io::Error) -> bool {
        error.get_ref().is_some_and(|cause| cause.is::<Self>())
    }
}

impl std::fmt::Display for UpdateCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("interrupted by Ctrl+C before replacement")?;
        if let Some(staging) = &self.0 {
            write!(formatter, ". Staging data kept at {}", staging.display())?;
        }
        Ok(())
    }
}

impl std::error::Error for UpdateCancelled {}

fn cancelled(staging: Option<&Path>) -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        UpdateCancelled(staging.map(Path::to_owned)),
    )
}
