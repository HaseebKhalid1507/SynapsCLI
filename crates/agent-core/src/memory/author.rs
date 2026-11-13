//! Who a run is: the identity a chat or one of its workers carries in its
//! memory binding and that `tool.call` forwards to extensions as `caller`.
//! An identity, not a secret and not a proof of who calls.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Author {
    pub actor: String,
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

impl Author {
    pub fn fresh() -> Self {
        Self {
            actor: format!("actor-{}", uuid::Uuid::new_v4().simple()),
            group: format!("group-{}", uuid::Uuid::new_v4().simple()),
            parent: None,
        }
    }

    pub fn child(&self) -> Self {
        Self {
            actor: format!("actor-{}", uuid::Uuid::new_v4().simple()),
            group: self.group.clone(),
            parent: Some(self.actor.clone()),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !prefixed_hex(&self.actor, "actor-", 32)
            || !prefixed_hex(&self.group, "group-", 32)
            || self
                .parent
                .as_ref()
                .is_some_and(|p| !prefixed_hex(p, "actor-", 32) || p == &self.actor)
        {
            return Err("invalid author".into());
        }
        Ok(())
    }
}

impl Default for Author {
    fn default() -> Self {
        Self::fresh()
    }
}

fn prefixed_hex(s: &str, prefix: &str, length: usize) -> bool {
    s.strip_prefix(prefix).is_some_and(|tail| {
        tail.len() == length
            && tail
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_keeps_group_and_names_parent() {
        let a = Author::fresh();
        a.validate().unwrap();
        let child = a.child();
        assert_eq!(a.group, child.group);
        assert_eq!(child.parent, Some(a.actor.clone()));
        assert_ne!(a.actor, child.actor);
        child.validate().unwrap();
    }

    #[test]
    fn malformed_authors_do_not_validate() {
        let good = Author::fresh();
        for bad in [
            Author { actor: "actor-x".into(), ..good.clone() },
            Author { group: format!("group-{}", "A".repeat(32)), ..good.clone() },
            Author { parent: Some(good.actor.clone()), ..good.clone() },
            Author { parent: Some("group-0".into()), ..good.clone() },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn serialises_without_parent_for_a_chat() {
        let a = Author::fresh();
        let v = serde_json::to_value(&a).unwrap();
        assert!(v.get("parent").is_none());
        assert_eq!(serde_json::from_value::<Author>(v).unwrap(), a);
    }
}
