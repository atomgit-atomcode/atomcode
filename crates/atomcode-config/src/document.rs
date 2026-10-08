//! A document the host keeps for the libraries, rather than a file they open.
//!
//! Some of what this product keeps holds secrets — MCP servers' tokens and
//! headers, OAuth tokens. A host that embeds the libraries may keep such a
//! document its own way: encrypted on disk, in a keychain, in a database. It
//! hands a [`DocumentStore`] in, and the library reads and edits the text
//! through it without knowing where or how the text is kept.
//!
//! Two operations, and the second is the reason for the shape:
//!
//! - [`DocumentStore::read`] — the document's text, or `None` when there is none.
//! - [`DocumentStore::update`] — read, edit, write back **as one step** that no
//!   other writer interleaves with. These documents are edited by reading the
//!   whole text, changing one entry and writing the whole text back. With a bare
//!   `write`, two editors (two runtimes in one process, a CLI beside a daemon)
//!   each write back what they read, and the one that writes last silently drops
//!   the other's change — a refreshed token lost, an "always allow" undone. Only
//!   the host knows what makes the step exclusive (a file lock, a mutex, a
//!   transaction), so the library hands it the edit and the host runs it inside
//!   whatever lock it has. Writing is an edit that ignores the old text;
//!   deleting is an edit that returns `None`.
//!
//! The edit is a pure transformation of text — no I/O, nothing awaited — so
//! holding a lock around it is cheap.

/// What a [`DocumentStore`]'s operations return. Any `std::error::Error` a host's
/// storage or decryption raises converts into it with `?`.
pub type DocumentResult<T> = anyhow::Result<T>;

/// A text document the host keeps (see the module docs).
pub trait DocumentStore: Send + Sync + std::fmt::Debug {
    /// The document's text, or `None` when it does not exist.
    ///
    /// An error is reported to whoever asked, with what the host said — never
    /// read as "no document": a caller that took an unreadable document for an
    /// empty one would write an empty one back over it.
    fn read(&self) -> DocumentResult<Option<String>>;

    /// Read the document, hand its text (`None` when it does not exist) to
    /// `edit`, and store what `edit` returns — `None` deletes it — with no other
    /// write in between.
    ///
    /// When `edit` fails, or reading fails, nothing is stored and the error is
    /// returned.
    fn update(
        &self,
        edit: &mut dyn FnMut(Option<&str>) -> DocumentResult<Option<String>>,
    ) -> DocumentResult<()>;
}
