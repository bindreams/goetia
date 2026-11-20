//! PROBE ONLY (branch `probe/task-principal`, never merged): what Task Scheduler stores of a task's
//! principal and what goetia's own readers make of it.

use super::api::TaskApi as _;
use super::read::known_account;
use super::write::stored_account;
use super::{Com, TaskPath};
use crate::backend::scm::manager::identity;
use crate::backend::scm::sid::sid_display;
use crate::backend::scm::task_xml::TaskXml;
use crate::spec::Id;

fn between<'a>(text: &'a str, open: &str, close: &str) -> &'a str {
    text.find(open)
        .and_then(|start| {
            let rest = &text[start..];
            rest.find(close).map(|end| &rest[..end + close.len()])
        })
        .unwrap_or("<absent>")
}

/// A report of the principal of the task `id`.
pub fn probe_principal(id: &Id) -> String {
    let path = TaskPath::of(id);
    let registered = match Com.get(&path) {
        Ok(Some(registered)) => registered,
        Ok(None) => return "task not installed".to_owned(),
        Err(e) => return format!("read failed: {}", e.report()),
    };
    let mut out = String::new();
    out.push_str(&format!(
        "Xml <Principals>: {}\n",
        between(&registered.xml, "<Principals>", "</Principals>")
    ));
    let principal = TaskXml::principal(&registered.xml);
    out.push_str(&format!("TaskXml::principal: {principal:?}\n"));
    out.push_str(&format!("blob account (stored_account): {:?}\n", stored_account(&registered)));
    if let Ok(Some(account)) = &principal {
        let text = match account {
            crate::backend::scm::instance::TaskAccount::Service(text)
            | crate::backend::scm::instance::TaskAccount::User(text) => text,
        };
        let resolved = identity::sid_of_principal(text);
        match &resolved {
            Ok(sid) => {
                out.push_str(&format!("sid_of_principal({text:?}): Ok({})\n", sid_display(sid)));
                out.push_str(&format!(
                    "  account_name_of_sid: {:?}\n",
                    identity::account_name_of_sid(sid).map_err(|e| e.report().to_string())
                ));
            }
            Err(e) => out.push_str(&format!("sid_of_principal({text:?}): Err({e:?}) / {}\n", e.report())),
        }
        let known = known_account(identity::sid_of_principal(text));
        out.push_str(&format!(
            "stored() principal (known_account): {}\n",
            match known {
                Ok(Some(sid)) => format!("Some({})", sid_display(&sid)),
                Ok(None) => "None (no such account)".to_owned(),
                Err(e) => format!("Err({})", e.report()),
            }
        ));
    }
    match Com.info(&path) {
        Ok(info) => out.push_str(&format!("info: {info:?}\n")),
        Err(e) => out.push_str(&format!("info failed: {}\n", e.report())),
    }
    out
}
