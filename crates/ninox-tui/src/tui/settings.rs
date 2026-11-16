//! The Settings tab's view state over the shared settings form
//! (`ninox_ui_shared::settings_form`).

pub use ninox_ui_shared::settings_form::*;
use ninox_core::config::AppConfig;

/// Client state of the Settings tab.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsView {
    pub fields: Vec<Field>,
    pub selected: usize,
    /// The inline editor's buffer while a Text field is being edited.
    pub editing: Option<String>,
    /// Why the last change was refused; shown under the field.
    pub error: Option<String>,
    /// `config.toml` did not parse: nothing is saved until it does.
    pub load_error: Option<String>,
}

impl SettingsView {
    pub fn selected_field(&self) -> Option<&Field> {
        self.fields.get(self.selected)
    }

    pub fn reload(&mut self, loaded: Result<AppConfig, String>) {
        let id = self.selected_field().map(|f| f.id.clone());
        match loaded {
            Ok(cfg) => {
                self.fields = fields(&cfg);
                self.load_error = None;
            }
            Err(e) => {
                self.fields = fields(&AppConfig::default());
                self.load_error = Some(e);
            }
        }
        self.selected = id
            .and_then(|id| self.fields.iter().position(|f| f.id == id))
            .unwrap_or(self.selected)
            .min(self.fields.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_keeps_the_selection_by_field() {
        let mut v = SettingsView::default();
        v.reload(Ok(AppConfig::default()));
        v.selected = v.fields.iter().position(|f| f.id == FieldId::Port).unwrap();
        let mut cfg = AppConfig::default();
        cfg.toggle_harness("codex");
        cfg.harnesses.insert("zzz".into(), Default::default());
        v.reload(Ok(cfg));
        assert_eq!(v.selected_field().map(|f| &f.id), Some(&FieldId::Port));
        v.reload(Err("bad".into()));
        assert_eq!(v.load_error.as_deref(), Some("bad"));
    }

    #[test]
    fn session_limits_take_a_number_unlimited_or_zero() {
        let mut cfg = AppConfig::default();
        let set = |cfg: &mut AppConfig, text: &str| apply(cfg, &Change::Set { id: FieldId::SessionLimit(None), text: text.into() });
        set(&mut cfg, "3").unwrap();
        assert_eq!(cfg.limits.limit_for(None), Some(3));
        assert!(set(&mut cfg, "lots").unwrap_err().contains("not a whole number"));
        assert_eq!(cfg.limits.limit_for(None), Some(3), "a refused value changes nothing");
        set(&mut cfg, "UNLIMITED").unwrap();
        assert_eq!(cfg.limits.limit_for(None), None);
        set(&mut cfg, "5").unwrap();
        set(&mut cfg, "0").unwrap();
        assert_eq!(cfg.limits.limit_for(None), None);
    }
}
