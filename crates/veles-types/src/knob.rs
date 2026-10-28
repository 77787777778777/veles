//! The config-surface `Knob` type (Phase 3, D2). A plugin's `--describe`
//! carries its config knobs so the setup UIs and the installer render a
//! form generically, with no core dependency. Core re-exports this at
//! `veles_core::config_surface::Knob`; the FILL logic (which reads
//! `VelesConfig` and writes TOML) stays in core.

/// One option on the surface.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Knob {
    /// Dotted TOML path: `agent.compaction.keep_recent`.
    pub path: String,
    /// The default, rendered as a TOML literal. `None` means the option
    /// is unset by default and must not be written with a value.
    pub default: Option<String>,
    /// What writing it would look like.
    pub shape: Shape,
    /// Why an option is left unwritten, when "unset by default" is not
    /// the whole truth.
    pub note: Option<String>,
    /// The environment variable that beats this option, when one exists.
    /// Precedence, not a synonym: being named here does NOT make a var
    /// `.env`-settable — only a reader that consults the dotenv file can
    /// be, which is what each connector's `env_surface` lists separately.
    pub env: Option<String>,
}

/// The TOML shape of an option, which decides whether a fill writes it or
/// only names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// A scalar or an inline array: `key = value`.
    Value,
    /// An empty array whose elements are scalars: `key = []`.
    ScalarArray,
    /// An array of tables: only ever `[[key]]`, which a fill must not
    /// pre-empt by writing `key = []`.
    TableArray,
    /// Unset by default, so there is no value to write.
    Unset,
    /// Has a default, and writing it would still be wrong: something else
    /// decides until the user opts in.
    Documented,
}

impl Knob {
    /// The section this knob lives in (`""` for a top-level key).
    pub fn section(&self) -> &str {
        match self.path.rsplit_once('.') {
            Some((head, _)) => head,
            None => "",
        }
    }

    /// The bare key inside its section.
    pub fn key(&self) -> &str {
        self.path.rsplit('.').next().unwrap_or(&self.path)
    }

    /// Does a fill write this one, or only name it in a comment?
    pub fn writable(&self) -> bool {
        matches!(self.shape, Shape::Value | Shape::ScalarArray) && self.default.is_some()
    }

    /// The comment that names an option a fill will not write. A known
    /// default goes in the line, so uncommenting it produces a working key
    /// rather than a parse error.
    pub fn comment(&self) -> String {
        let note = self
            .note
            .clone()
            .or_else(|| self.env.clone().map(|v| format!("{v} wins over this")))
            .unwrap_or_else(|| {
                if self.writable() {
                    "default".to_string()
                } else {
                    "unset by default".to_string()
                }
            });
        let value = self.default.as_deref().unwrap_or("");
        match self.shape {
            Shape::TableArray => format!("# [[{}]]          # {note}", self.path),
            _ => format!("# {} = {value:<10} # {note}", self.key()),
        }
    }

    /// An option a connector reads out of its own blob.
    pub fn blob(path: &str, default: Option<String>, note: Option<&str>) -> Knob {
        let shape = match &default {
            None => Shape::Unset,
            Some(_) => Shape::Value,
        };
        Knob {
            path: path.to_string(),
            default,
            shape,
            note: note.map(str::to_string),
            env: None,
        }
    }

    /// A blob option with a default that a fill must show but not write,
    /// because something else wins until the user opts in.
    pub fn blob_documented(path: &str, default: &str, note: &str) -> Knob {
        Knob {
            path: path.to_string(),
            default: Some(default.to_string()),
            shape: Shape::Documented,
            note: Some(note.to_string()),
            env: None,
        }
    }

    /// A blob option an environment variable beats, so a fill shows the
    /// default and leaves the decision to the var.
    pub fn blob_env(path: &str, default: Option<&str>, env: &str, note: Option<&str>) -> Knob {
        Knob {
            path: path.to_string(),
            default: default.map(str::to_string),
            shape: match default {
                None => Shape::Unset,
                Some(_) => Shape::Documented,
            },
            note: note.map(str::to_string),
            env: Some(env.to_string()),
        }
    }

    /// An option that is only ever an array of tables, so a fill names it
    /// and never writes `= []`.
    pub fn blob_array(path: &str) -> Knob {
        Knob {
            path: path.to_string(),
            default: None,
            shape: Shape::TableArray,
            note: None,
            env: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_round_trips_through_the_manifest_wire_format() {
        for shape in [
            Shape::Value,
            Shape::ScalarArray,
            Shape::TableArray,
            Shape::Unset,
            Shape::Documented,
        ] {
            let k = Knob {
                path: "platforms.example.option".into(),
                default: None,
                shape,
                note: Some("installer form".into()),
                env: None,
            };
            let encoded = serde_json::to_string(&k).unwrap();
            let decoded: Knob = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, k);
        }
    }

    #[test]
    fn a_documented_knob_is_named_not_written() {
        let k = Knob::blob_documented("platforms.atproto.pds_url", "\"http://x\"", "the PDS");
        assert!(!k.writable());
        assert_eq!(k.section(), "platforms.atproto");
        assert_eq!(k.key(), "pds_url");
        assert!(k.comment().contains("pds_url") && k.comment().contains("the PDS"));
    }

    #[test]
    fn a_writable_knob_writes() {
        let k = Knob::blob("agent.model", Some("\"gpt-4o\"".into()), None);
        assert!(k.writable());
        assert_eq!(k.section(), "agent");
    }

    #[test]
    fn an_env_knob_says_the_var_wins() {
        let k = Knob::blob_env("platforms.irc.server", None, "IRC_SERVER", None);
        assert!(k.comment().contains("IRC_SERVER wins"));
    }

    #[test]
    fn a_table_array_is_never_an_empty_write() {
        let k = Knob::blob_array("platforms.irc.networks");
        assert!(!k.writable());
        assert!(k.comment().starts_with("# [[platforms.irc.networks]]"));
    }
}
