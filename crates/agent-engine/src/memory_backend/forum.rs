//! Typed forum routing through the captured Axel memory backend. No fallback.
use super::{error, MemoryBinding, USER_SCOPE};
use crate::Result;
use agent_core::memory::forum::{self, Envelope, Page, Post, PostRequest, Read, Receipt, Status};
use serde_json::json;

impl MemoryBinding {
    fn require_forum(&self) -> Result<()> {
        if !self.is_axel() || self.scope()?.key() == USER_SCOPE {
            return Err(error("project forum requires selected Axel repository memory; unavailable for legacy/user scope; no fallback"));
        }
        self.forum_author().validate().map_err(error)
    }
    pub async fn forum_post(&self, post: Post) -> Result<Receipt> {
        self.require_forum()?;
        let expected = Envelope::new(self.scope()?.key(), self.forum_author().clone(), &post)
            .map_err(error)?;
        let request = PostRequest {
            author: self.forum_author().clone(),
            post,
        };
        let value = self.call("forum_post",serde_json::to_value(request).map_err(|_|error("forum serialization failed"))?).await
            .map_err(|cause|error(format!("forum post has no success receipt for {}: {cause}. Do not claim publication. If the outcome is uncertain, reconcile first; any exact retry must keep the original request key, payload and host author",expected.id())))?;
        let receipt: Receipt = serde_json::from_value(value).map_err(|_| {
            error(format!(
                "invalid forum acknowledgement; commit unknown for {}",
                expected.id()
            ))
        })?;
        if receipt.id != expected.id()
            || receipt.thread_id != expected.thread_id
            || receipt.digest != expected.digest
            || (receipt.status != Status::Tombstoned && receipt.timestamp_ms.is_none())
            || receipt.timestamp_ms.is_some_and(|n| n > i64::MAX as u64)
        {
            return Err(error(format!(
                "forum acknowledgement mismatch; commit unknown for {}",
                expected.id()
            )));
        }
        Ok(receipt)
    }
    pub async fn forum_read(&self, query: Read) -> Result<Page> {
        self.require_forum()?;
        query.validate().map_err(error)?;
        let value = self
            .call(
                "forum_read",
                serde_json::to_value(&query)
                    .map_err(|_| error("forum query serialization failed"))?,
            )
            .await?;
        if serde_json::to_vec(&value)
            .map_err(|_| error("forum result serialization failed"))?
            .len()
            > forum::PAGE_BYTES
        {
            return Err(error("forum page exceeds byte limit"));
        }
        let page: Page = serde_json::from_value(value).map_err(|_| error("invalid forum page"))?;
        let projects = page
            .entries
            .iter()
            .map(|e| e.envelope.project.as_str())
            .collect::<Vec<_>>();
        let members = self.authorized_projects(&projects).await?;
        validate_page(&page, &query, &members)?;
        Ok(page)
    }
    pub async fn forum_forget(&self, id: &str) -> Result<()> {
        self.require_forum()?;
        if !forum::valid_id(id) {
            return Err(error("forum forget requires an exact msg- ID"));
        }
        match self.call("forum_forget", json!({"id":id})).await? {
            serde_json::Value::Bool(true) => Ok(()),
            serde_json::Value::Bool(false) => Err(error("forum post unavailable in this project")),
            _ => Err(error(
                "invalid forum forget acknowledgement; outcome unknown",
            )),
        }
    }
}
fn validate_page(page: &Page, query: &Read, members: &[String]) -> Result<()> {
    if page.entries.len() > query.limit
        || page.entries.iter().map(|e| e.body.len()).sum::<usize>() > forum::PAGE_BODY_BYTES
    {
        return Err(error("forum page exceeds entry/body bound"));
    }
    let mut previous = query.after.clone();
    for entry in &page.entries {
        entry.validate().map_err(error)?;
        let cursor = entry.cursor();
        if !members.contains(&entry.envelope.project)
            || previous.as_ref().is_some_and(|p| cursor <= *p)
            || match &query.thread_id {
                Some(id) => entry.envelope.thread_id != *id || entry.truncated,
                None => !entry.envelope.is_root(),
            }
        {
            return Err(error("forum page violates scope/order/thread constraints"));
        }
        previous = Some(cursor);
    }
    if let Some(next) = &page.next {
        next.validate().map_err(error)?;
        if page.entries.last().map(|e| e.cursor()).as_ref() != Some(next) {
            return Err(error("forum cursor skips unreturned entries"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use forum::{Author, Entry};
    fn entry() -> Entry {
        let post = Post {
            request_key: "one".into(),
            thread_id: None,
            reply_to: None,
            title: "Finding".into(),
            body: "synthetic note".into(),
            retention_days: 30,
        };
        let envelope = Envelope::new("p1111111111111111", Author::fresh(), &post).unwrap();
        Entry {
            id: envelope.id(),
            timestamp_ms: 1,
            envelope,
            body: post.body,
            truncated: false,
        }
    }
    #[test]
    fn validates_scope_order_digest_and_cursor() {
        let row = entry();
        let scope = vec![row.envelope.project.clone()];
        let q = Read::default();
        let mut p = Page {
            entries: vec![row.clone()],
            next: Some(row.cursor()),
        };
        validate_page(&p, &q, &scope).unwrap();
        assert!(validate_page(&p, &q, &["p2222222222222222".into()]).is_err());
        p.entries[0].body.push('!');
        assert!(validate_page(&p, &q, &scope).is_err());
        p.entries[0] = row.clone();
        p.entries.push(row.clone());
        assert!(validate_page(&p, &q, &scope).is_err());
        p.entries.pop();
        p.next.as_mut().unwrap().timestamp_ms += 1;
        assert!(validate_page(&p, &q, &scope).is_err());
    }
    #[test]
    fn rejects_truncated_thread_reads_and_invalid_root_listing() {
        let row = entry();
        let scope = vec![row.envelope.project.clone()];
        let q = Read {
            thread_id: Some(row.id.clone()),
            ..Default::default()
        };
        let mut p = Page {
            entries: vec![row],
            next: None,
        };
        validate_page(&p, &q, &scope).unwrap();
        p.entries[0].truncated = true;
        assert!(validate_page(&p, &q, &scope).is_err());
    }
}

#[cfg(all(test, unix))]
#[path = "forum_integration_tests.rs"]
mod integration_tests;
