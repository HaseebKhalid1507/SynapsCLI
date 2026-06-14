//! Apply disable lists to discovered skills.

use crate::skills::LoadedSkill;

pub fn filter_disabled(
    skills: Vec<LoadedSkill>,
    disabled_plugins: &[String],
    disabled_skills: &[String],
) -> Vec<LoadedSkill> {
    skills.into_iter().filter(|s| {
        if let Some(ref p) = s.plugin {
            if disabled_plugins.iter().any(|d| d == p) {
                tracing::debug!("skill '{}' disabled via disabled_plugins='{}'", s.name, p);
                return false;
            }
        }
        if disabled_skills.iter().any(|d| d == &s.name) {
            tracing::debug!("skill '{}' disabled via disabled_skills (bare)", s.name);
            return false;
        }
        if let Some(ref p) = s.plugin {
            let qualified = format!("{}:{}", p, s.name);
            if disabled_skills.iter().any(|d| d == &qualified) {
                tracing::debug!("skill '{}' disabled via disabled_skills (qualified)", qualified);
                return false;
            }
        }
        true
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk_skill(name: &str, plugin: Option<&str>) -> LoadedSkill {
        LoadedSkill {
            name: name.to_string(),
            description: String::new(),
            body: String::new(),
            plugin: plugin.map(str::to_string),
            base_dir: PathBuf::from("/"),
            source_path: PathBuf::from("/SKILL.md"),
        }
    }

    #[test]
    fn disable_by_plugin() {
        let s = vec![mk_skill("a", Some("p1")), mk_skill("b", Some("p2"))];
        let out = filter_disabled(s, &["p1".to_string()], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "b");
    }

    #[test]
    fn disable_by_bare_name() {
        let s = vec![mk_skill("a", Some("p1")), mk_skill("a", Some("p2")), mk_skill("b", None)];
        let out = filter_disabled(s, &[], &["a".to_string()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "b");
    }

    #[test]
    fn disable_by_qualified_name() {
        let s = vec![mk_skill("a", Some("p1")), mk_skill("a", Some("p2"))];
        let out = filter_disabled(s, &[], &["p1:a".to_string()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].plugin.as_deref(), Some("p2"));
    }

    #[test]
    fn empty_filters_pass_through() {
        let s = vec![mk_skill("a", None), mk_skill("b", Some("p"))];
        let out = filter_disabled(s, &[], &[]);
        assert_eq!(out.len(), 2);
    }
}
