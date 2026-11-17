//! The Settings tab's view state over the shared settings form
//! (`ninox_ui_shared::settings_form`).

pub use ninox_ui_shared::settings_form::*;
use ninox_core::config::AppConfig;

/// Client state of the Settings page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsView {
    pub fields: Vec<Field>,
    /// The section shown on the right; `selected` is always one of its fields.
    pub section: SettingsSection,
    /// `true`: arrows move within the fields; `false`: within the section bar.
    pub in_fields: bool,
    /// Index into `fields`.
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

    /// Indices into `fields` of the current section's fields. The file
    /// button (`FieldId::OpenFile`) is in no section: the page header has it.
    pub fn section_fields(&self) -> Vec<usize> {
        self.fields.iter().enumerate().filter(|(_, f)| f.id.section() == Some(self.section)).map(|(i, _)| i).collect()
    }

    pub fn select_section(&mut self, section: SettingsSection) {
        self.section = section;
        self.editing = None;
        self.error = None;
        self.selected = self.section_fields().first().copied().unwrap_or(0);
    }

    /// Step the section bar by `delta` through `SettingsSection::GROUPS` order.
    pub fn move_section(&mut self, delta: i64) {
        let all = SettingsSection::all();
        let at = all.iter().position(|s| *s == self.section).unwrap_or(0) as i64;
        let next = all[(at + delta).clamp(0, all.len() as i64 - 1) as usize];
        if next != self.section {
            self.select_section(next);
        }
    }

    /// Move the field selection within the current section.
    pub fn move_field(&mut self, delta: i64) {
        let visible = self.section_fields();
        if visible.is_empty() {
            return;
        }
        let at = visible.iter().position(|&i| i == self.selected).unwrap_or(0) as i64;
        if let Some(&i) = visible.get((at + delta).clamp(0, visible.len() as i64 - 1) as usize) {
            self.selected = i;
        }
        self.error = None;
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
        if self.selected_field().is_none_or(|f| f.id.section() != Some(self.section)) {
            self.selected = self.section_fields().first().copied().unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_keeps_the_selection_by_field() {
        let mut v = SettingsView::default();
        v.reload(Ok(AppConfig::default()));
        v.section = SettingsSection::Engine;
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
