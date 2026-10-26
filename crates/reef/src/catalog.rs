//! The fleet's label catalog, which Reef keeps (`reef-labels.json` in the
//! data dir) and desktops in reef mode and phones read. Labels belong to
//! agents (see [`fleet::labels`]); this module only keeps the catalog
//! itself current: the one-time import from the hosts' Paseo labels, names
//! seen later, and edits.

use std::path::Path;
use std::time::{Duration, Instant};

use fleet::Fleet;
use fleet::labels::{Catalog, ImportHost, LabelId};
use serde_json::{Value, json};

/// The catalog's file in the data dir.
pub const FILE: &str = "reef-labels.json";

/// How long after start the import waits for hosts still loading; after
/// it, hosts not in yet have their labels adopted as they arrive.
pub const IMPORT_WAIT: Duration = Duration::from_secs(20);

/// The saved catalog, or an empty one not yet imported.
pub fn load(path: &Path) -> Catalog {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, catalog: &Catalog) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(catalog).map_err(|err| err.to_string())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|err| format!("{}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|err| format!("{}: {err}", path.display()))
}

/// Brings the catalog up to date with the fleet: the import, once every
/// online host has loaded or [`IMPORT_WAIT`] has passed, then any Paseo
/// workspace label no alias knows yet. Returns whether it changed.
pub fn keep_up(fleet: &mut Fleet, started: Instant, now: Instant) -> bool {
    let Some(catalog) = fleet.labels.as_ref() else {
        return false;
    };
    let before = catalog.revision;
    let ready = |host: &fleet::Host| host.directory.hydrated && host.directory.agents_hydrated;
    if !catalog.imported {
        let waited = now.duration_since(started) >= IMPORT_WAIT;
        let all_in = fleet
            .hosts
            .iter()
            .all(|host| ready(host) || !host.connection.is_online())
            && fleet.hosts.iter().any(ready);
        if !(waited || all_in) {
            return false;
        }
        let hosts: Vec<ImportHost> = (fleet.hosts.iter())
            .filter(|host| ready(host))
            .map(|host| ImportHost {
                labels: &host.directory.labels,
                workspaces: host.directory.workspaces.values().collect(),
            })
            .collect();
        let mut catalog = fleet.labels.take().expect("checked above");
        catalog.import(&hosts, |name| {
            crate::lists::label_color_name(name).to_string()
        });
        fleet.labels = Some(catalog);
    }
    let mut catalog = fleet.labels.take().expect("checked above");
    for host in fleet.hosts.iter().filter(|host| ready(host)) {
        for def in &host.directory.labels {
            catalog.adopt(&def.name, &def.color);
        }
        for workspace in host.directory.workspaces.values() {
            for name in &workspace.labels {
                catalog.adopt(name, crate::lists::label_color_name(name));
            }
        }
    }
    let changed = catalog.revision != before;
    fleet.labels = Some(catalog);
    changed
}

/// The labels named `names`, made where the catalog has none of that name,
/// in a colour of their own. Names that cannot be labels are left out.
pub fn resolve_names(catalog: &mut Catalog, names: &[String]) -> Vec<LabelId> {
    let mut ids: Vec<LabelId> = Vec::new();
    for name in names {
        let id = match catalog.by_name(name) {
            Some(label) => Some(label.id.clone()),
            None => catalog
                .create(name, crate::lists::label_color_name(name))
                .ok(),
        };
        if let Some(id) = id
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    ids
}

/// One edit from a desktop, `POST /api/labels`: `{"op": "create", "name",
/// "color"?}`, `{"op": "rename", "id", "name"}`, `{"op": "recolor", "id",
/// "color"}` or `{"op": "delete", "id"}`. Answers what changed and the
/// catalog's new revision.
pub fn edit(catalog: &mut Catalog, edit: &Value) -> Result<Value, String> {
    let text = |key: &str| edit[key].as_str().ok_or(format!("{key} is missing"));
    let id = || text("id").map(LabelId::new);
    let color = |key: &str| -> Result<String, String> {
        let color = text(key)?;
        if crate::lists::is_label_color(color) {
            Ok(color.to_string())
        } else {
            Err(format!("{color} is not a label colour"))
        }
    };
    let answer = match edit["op"].as_str() {
        Some("create") => {
            let name = text("name")?;
            // Without one, the colour its name picks, as a phone's new label gets.
            let color = match edit.get("color") {
                Some(_) => color("color")?,
                None => crate::lists::label_color_name(name).to_string(),
            };
            let id = catalog.create(name, &color)?;
            json!({ "id": id })
        }
        Some("rename") => {
            catalog.rename(&id()?, text("name")?)?;
            json!({})
        }
        Some("recolor") => {
            catalog.recolor(&id()?, &color("color")?)?;
            json!({})
        }
        Some("delete") => {
            catalog.delete(&id()?)?;
            json!({})
        }
        _ => return Err("op is not create, rename, recolor or delete".into()),
    };
    let mut answer = answer;
    answer["revision"] = json!(catalog.revision);
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet::HostConfig;

    fn fleet_with(labels: Value, workspaces: Value) -> Fleet {
        let mut fleet = Fleet::new([HostConfig {
            id: fleet::HostId::new("a"),
            name: "a".into(),
            ..Default::default()
        }]);
        let dir = &mut fleet.hosts[0].directory;
        dir.apply_labels_reply(&json!({ "labels": labels }));
        dir.apply_workspaces_reply(&json!({ "entries": workspaces }), true);
        dir.agents_hydrated = true;
        fleet.hosts[0].connection = fleet::Connection::Online;
        fleet.labels = Some(Catalog::default());
        fleet
    }

    #[test]
    fn the_import_waits_for_loaded_hosts_then_later_names_are_adopted() {
        let start = Instant::now();
        let mut fleet = fleet_with(json!([{"name": "ux", "color": "blue"}]), json!([]));
        fleet.hosts[0].directory.agents_hydrated = false;
        assert!(!keep_up(&mut fleet, start, start), "still loading");
        fleet.hosts[0].directory.agents_hydrated = true;
        assert!(keep_up(&mut fleet, start, start));
        let catalog = fleet.labels.as_ref().unwrap();
        assert!(catalog.imported);
        assert_eq!(catalog.by_name("ux").unwrap().color, "blue");
        assert!(!keep_up(&mut fleet, start, start), "nothing new");
        // A workspace label made later, by Paseo's app.
        fleet.hosts[0]
            .directory
            .apply_push(&json!({"type": "workspace_update", "payload":
            {"kind": "upsert", "workspace": {"id": "w", "labels": ["docs"]}}}));
        assert!(keep_up(&mut fleet, start, start));
        assert!(
            fleet
                .labels
                .as_ref()
                .unwrap()
                .by_paseo_name("docs")
                .is_some()
        );
    }

    #[test]
    fn edits_and_names_resolve_against_the_catalog() {
        let mut catalog = Catalog::default();
        let made = edit(
            &mut catalog,
            &json!({"op": "create", "name": "ux", "color": "blue"}),
        )
        .unwrap();
        let id = made["id"].as_str().unwrap().to_string();
        assert_eq!(made["revision"], 1);
        assert!(
            edit(
                &mut catalog,
                &json!({"op": "create", "name": "x", "color": "#123456"})
            )
            .is_err()
        );
        let picked = edit(&mut catalog, &json!({"op": "create", "name": "picked"})).unwrap();
        let picked = catalog
            .get(&LabelId::new(picked["id"].as_str().unwrap()))
            .unwrap();
        assert_eq!(picked.color, crate::lists::label_color_name("picked"));
        edit(
            &mut catalog,
            &json!({"op": "rename", "id": id, "name": "Design"}),
        )
        .unwrap();
        edit(
            &mut catalog,
            &json!({"op": "recolor", "id": id, "color": "red"}),
        )
        .unwrap();
        let ids = resolve_names(
            &mut catalog,
            &["design".into(), "new one".into(), "Design".into()],
        );
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].as_str(), id);
        assert_eq!(catalog.get(&ids[1]).unwrap().name, "new one");
        edit(&mut catalog, &json!({"op": "delete", "id": id})).unwrap();
        assert!(catalog.get(&LabelId::new(&id)).is_none());
        assert!(edit(&mut catalog, &json!({"op": "nope"})).is_err());
    }
}
