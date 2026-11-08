//! Everything that touches a real volume.

mod change;
mod iter;
mod volume;

pub use change::{ChangeReader, ChangeStream, Watcher};
pub use iter::{MftEntries, PathResolver, Records, Scan};
pub use volume::Volume;

macro_rules! opaque_debug {
    ($($name:ty => $label:literal),* $(,)?) => {
        $(impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct($label).finish_non_exhaustive()
            }
        })*
    };
}

opaque_debug! {
    ChangeReader => "ChangeReader",
    ChangeStream => "ChangeStream",
    Watcher => "Watcher",
    MftEntries<'_> => "MftEntries",
    PathResolver<'_> => "PathResolver",
    Records<'_> => "Records",
    Scan<'_> => "Scan",
}
