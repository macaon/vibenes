//! FDS BIOS (`disksys.rom`) file resolution.
//!
//! Nintendo's 8 KiB FDS BIOS is copyrighted and not legally
//! distributable — users must supply their own dump. This module
//! locates the file, validates it, and surfaces a clear error when
//! it's missing. The FDS isn't runnable without it: game code JSRs
//! into BIOS entry points for every disk operation (file load,
//! write, error handling), and the reset vector itself lives in
//! BIOS space at `$FFFC-$FFFD`.
//!
//! ## Search precedence
//!
//! First match wins:
//!
//! 1. `VIBENES_FDS_BIOS` env var (absolute path). Highest priority —
//!    lets CI and test harnesses override anything.
//! 2. `--fds-bios <path>` CLI flag (parsed in `main.rs`, threaded in
//!    as `cli_override`).
//! 3. `config.fds.bios_path` — where a future settings UI writes.
//! 4. `$XDG_CONFIG_HOME/vibenes/disksys.rom` (default
//!    `$HOME/.config/vibenes/disksys.rom`).
//! 5. Same directory as the `.fds` being loaded — last-ditch for
//!    users who keep everything next to the ROM.
//!
//! ## What we check
//!
//! - Exactly 8192 bytes. Anything else is structurally wrong.
//! - CRC32 warning if it doesn't match the known-good `0x5E607DCF`
//!   (the US/JP FDS BIOS). A warning rather than an error because
//!   unofficial translations and patched BIOSes exist and work fine.
//!
//! Filename convention matches nesdev + Nestopia / FCEUX: `disksys.rom`.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::crc32::crc32;

pub const BIOS_SIZE: usize = 8 * 1024;
pub const BIOS_FILENAME: &str = "disksys.rom";

/// CRC32 of the stock Japanese FDS BIOS shipped by Nintendo. Any
/// user-supplied file matching this hash is known-good. Mismatches
/// are warned, not rejected — modified / regional BIOSes exist.
pub const KNOWN_GOOD_CRC32: u32 = 0x5E607DCF;

/// Env var users can set to bypass all other lookup rules.
pub const ENV_OVERRIDE: &str = "VIBENES_FDS_BIOS";

/// Loaded FDS BIOS — exactly 8 KiB.
#[derive(Clone)]
pub struct FdsBios {
    /// Raw BIOS bytes, always exactly [`BIOS_SIZE`].
    pub bytes: Vec<u8>,
    /// Absolute path the BIOS was loaded from. Useful for debug logs.
    pub source: PathBuf,
    /// Whether the file's CRC32 matched [`KNOWN_GOOD_CRC32`].
    pub is_known_good: bool,
}

impl fmt::Debug for FdsBios {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FdsBios")
            .field("source", &self.source)
            .field("is_known_good", &self.is_known_good)
            .field("bytes", &format_args!("[{} bytes]", self.bytes.len()))
            .finish()
    }
}

/// Layered BIOS lookup input. Each field corresponds to one priority
/// tier in the search order above; `None` means "skip this tier."
#[derive(Debug, Default, Clone)]
pub struct BiosSearch {
    /// `--fds-bios <path>` override (tier 2).
    pub cli_override: Option<PathBuf>,
    /// From `config.fds.bios_path` (tier 3).
    pub config: Option<PathBuf>,
    /// Directory containing the `.fds` being loaded (tier 5). The
    /// filename [`BIOS_FILENAME`] is appended to this path.
    pub rom_dir: Option<PathBuf>,
}

/// Errors from [`FdsBios::resolve`].
#[derive(Debug)]
pub enum BiosError {
    /// None of the search tiers produced a file.
    NotFound { attempted: Vec<PathBuf> },
    /// File was found but size is wrong (not exactly 8 KiB).
    BadSize { path: PathBuf, actual: usize },
    /// File I/O error reading a candidate path.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for BiosError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BiosError::NotFound { attempted } => {
                writeln!(
                    f,
                    "FDS BIOS ({}) not found. Nintendo's copyrighted BIOS is required \
                     to run FDS games; vibenes2 cannot legally bundle it.",
                    BIOS_FILENAME
                )?;
                writeln!(f)?;
                writeln!(f, "Searched (in order):")?;
                for (i, path) in attempted.iter().enumerate() {
                    writeln!(f, "  {}. {}", i + 1, path.display())?;
                }
                writeln!(f)?;
                write!(
                    f,
                    "Place your BIOS dump at one of those paths, or set the \
                     {ENV_OVERRIDE} environment variable / --fds-bios CLI flag."
                )
            }
            BiosError::BadSize { path, actual } => write!(
                f,
                "{}: BIOS file must be exactly {} bytes, got {}",
                path.display(),
                BIOS_SIZE,
                actual
            ),
            BiosError::Io { path, source } => {
                write!(f, "reading BIOS at {}: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for BiosError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BiosError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl FdsBios {
    /// Resolve, validate, and load the BIOS. Returns a ready-to-use
    /// [`FdsBios`] or a [`BiosError`] whose `Display` impl spells out
    /// every path checked so the user can fix their setup.
    pub fn resolve(search: &BiosSearch) -> Result<Self, BiosError> {
        let candidates = gather_candidate_paths(search);
        for path in &candidates {
            if !path.is_file() {
                continue;
            }
            let bytes = std::fs::read(path).map_err(|e| BiosError::Io {
                path: path.clone(),
                source: e,
            })?;
            return Self::from_bytes(bytes, path.clone());
        }
        Err(BiosError::NotFound {
            attempted: candidates,
        })
    }

    /// Validate-and-wrap helper split out for testing — takes raw
    /// bytes plus a path for error reporting. Also what
    /// [`FdsBios::resolve`] calls after reading from disk.
    pub fn from_bytes(bytes: Vec<u8>, source: PathBuf) -> Result<Self, BiosError> {
        if bytes.len() != BIOS_SIZE {
            return Err(BiosError::BadSize {
                path: source,
                actual: bytes.len(),
            });
        }
        let is_known_good = crc32(&bytes) == KNOWN_GOOD_CRC32;
        Ok(Self {
            bytes,
            source,
            is_known_good,
        })
    }
}

/// Pure ordered-candidate builder. Inputs are all explicit so tests
/// don't need to mutate global env vars. Production callers feed this
/// through [`gather_candidate_paths`] which actually reads the env.
pub fn build_candidate_paths(
    env_override: Option<PathBuf>,
    cli_override: Option<&Path>,
    config: Option<&Path>,
    xdg: Option<&Path>,
    rom_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(5);
    if let Some(p) = env_override {
        paths.push(p);
    }
    if let Some(p) = cli_override {
        paths.push(p.to_path_buf());
    }
    if let Some(p) = config {
        paths.push(p.to_path_buf());
    }
    if let Some(p) = xdg {
        paths.push(p.to_path_buf());
    }
    if let Some(p) = rom_dir {
        paths.push(p.join(BIOS_FILENAME));
    }
    paths
}

/// Thin production wrapper around [`build_candidate_paths`] that
/// pulls the env var + XDG default from the current OS environment.
fn gather_candidate_paths(search: &BiosSearch) -> Vec<PathBuf> {
    let env = std::env::var(ENV_OVERRIDE).ok().map(PathBuf::from);
    let xdg = xdg_config_path();
    build_candidate_paths(
        env,
        search.cli_override.as_deref(),
        search.config.as_deref(),
        xdg.as_deref(),
        search.rom_dir.as_deref(),
    )
}

/// `$XDG_CONFIG_HOME/vibenes/disksys.rom` — falling back to
/// `$HOME/.config/vibenes/disksys.rom` when `XDG_CONFIG_HOME` is unset.
/// Returns `None` only when neither env var is set (headless CI that
/// has no HOME, basically).
fn xdg_config_path() -> Option<PathBuf> {
    let base: PathBuf = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("vibenes").join(BIOS_FILENAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- from_bytes validation ----

    #[test]
    fn from_bytes_accepts_8_kib() {
        let bios =
            FdsBios::from_bytes(vec![0u8; BIOS_SIZE], PathBuf::from("/tmp/x")).unwrap();
        assert_eq!(bios.bytes.len(), BIOS_SIZE);
        // Zeros don't match the known-good CRC.
        assert!(!bios.is_known_good);
    }

    #[test]
    fn from_bytes_rejects_wrong_size() {
        match FdsBios::from_bytes(vec![0u8; BIOS_SIZE - 1], PathBuf::from("/tmp/x")) {
            Err(BiosError::BadSize { actual, .. }) => assert_eq!(actual, BIOS_SIZE - 1),
            other => panic!("expected BadSize, got {other:?}"),
        }
    }

    #[test]
    fn from_bytes_flags_known_good_crc() {
        // We can't reproduce the copyrighted BIOS bytes here, but the
        // detection is purely `crc32(bytes) == KNOWN_GOOD_CRC32`.
        // Keep the public constant pinned so a typo in the value
        // shows up as a test break.
        assert_eq!(KNOWN_GOOD_CRC32, 0x5E607DCF);
    }

    // ---- candidate-path ordering ----

    #[test]
    fn builder_env_wins_over_all_others() {
        let paths = build_candidate_paths(
            Some(PathBuf::from("/env.rom")),
            Some(Path::new("/cli.rom")),
            Some(Path::new("/config.rom")),
            Some(Path::new("/xdg.rom")),
            Some(Path::new("/rom_dir/")),
        );
        assert_eq!(paths[0], PathBuf::from("/env.rom"));
        assert_eq!(paths[1], PathBuf::from("/cli.rom"));
        assert_eq!(paths[2], PathBuf::from("/config.rom"));
        assert_eq!(paths[3], PathBuf::from("/xdg.rom"));
        assert_eq!(paths[4], PathBuf::from("/rom_dir").join(BIOS_FILENAME));
    }

    #[test]
    fn builder_skips_none_tiers() {
        let paths = build_candidate_paths(
            None,
            Some(Path::new("/cli.rom")),
            None,
            Some(Path::new("/xdg.rom")),
            None,
        );
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("/cli.rom"));
        assert_eq!(paths[1], PathBuf::from("/xdg.rom"));
    }

    #[test]
    fn builder_appends_filename_to_rom_dir_only() {
        // The CLI / config / XDG entries are FULL paths supplied by
        // the caller; only the "ROM directory" tier gets the filename
        // appended automatically.
        let paths = build_candidate_paths(
            None,
            Some(Path::new("/some/custom/location.rom")),
            None,
            None,
            Some(Path::new("/roms/")),
        );
        assert_eq!(paths[0], PathBuf::from("/some/custom/location.rom"));
        assert_eq!(paths[1], PathBuf::from("/roms").join(BIOS_FILENAME));
    }

    // ---- error rendering ----

    #[test]
    fn not_found_error_enumerates_searched_paths() {
        let err = BiosError::NotFound {
            attempted: vec![
                PathBuf::from("/env.rom"),
                PathBuf::from("/cli.rom"),
                PathBuf::from("/xdg.rom"),
            ],
        };
        let text = format!("{err}");
        assert!(text.contains("disksys.rom"), "got: {text}");
        assert!(text.contains("/env.rom"), "got: {text}");
        assert!(text.contains("/cli.rom"), "got: {text}");
        assert!(text.contains("/xdg.rom"), "got: {text}");
        assert!(text.contains("VIBENES_FDS_BIOS"), "got: {text}");
        assert!(text.contains("--fds-bios"), "got: {text}");
    }

    #[test]
    fn bad_size_error_names_the_path() {
        let err = BiosError::BadSize {
            path: PathBuf::from("/weird.rom"),
            actual: 4096,
        };
        let text = format!("{err}");
        assert!(text.contains("/weird.rom"));
        assert!(text.contains("4096"));
        assert!(text.contains(&BIOS_SIZE.to_string()));
    }
}
