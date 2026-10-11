// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin places <ENTITY_ID>...`: where entities are projected in the working
//! copy, for an editor that has to put a cursor on one.
//!
//! The answer comes from the daemon's `POST /projection/places`, which reads
//! the graph and nothing else (see [`kin_projection::places`]). This command
//! renders it and exits 0 whenever the daemon answered: each id carries its own
//! status, so an entity the graph cannot place is part of the answer rather
//! than an error. The command is hidden from `kin --help` because it is an
//! integration surface for editors, not a way to explore a repository: that is
//! `kin locate`, `kin refs` and the other entity commands.

use anyhow::{Context, Result};
use kin_projection::places::{EntityPlace, PlacesRequest, PlacesResponse, MAX_PLACES_PER_REQUEST};

pub async fn run(entity_ids: Vec<String>, json: bool) -> Result<()> {
    if entity_ids.len() > MAX_PLACES_PER_REQUEST {
        anyhow::bail!(
            "kin places answers at most {MAX_PLACES_PER_REQUEST} entities per call; split the {} ids into several calls",
            entity_ids.len()
        );
    }
    let layout = crate::commands::require_repository_layout()?;
    let client = crate::daemon_client::DaemonClient::connect_for_command("places", &layout).await?;
    let response = client
        .projection_places(&PlacesRequest { entity_ids })
        .await
        .context("daemon projection places failed")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        print!("{}", render(&response));
    }
    Ok(())
}

/// One line per entity: where it is, or why it has no place.
pub fn render(response: &PlacesResponse) -> String {
    let mut out = String::new();
    for place in &response.places {
        match place {
            EntityPlace::Projected {
                name,
                kind,
                path,
                start_line,
                start_column,
                ..
            } => {
                let kind = serde_json::to_value(kind)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "{name} [{kind}] {path}:{start_line}:{start_column}\n"
                ));
            }
            EntityPlace::NotProjected {
                entity_id, detail, ..
            } => out.push_str(&format!("{entity_id} not projected: {detail}\n")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::EntityKind;
    use kin_projection::places::{NotProjectedReason, COLUMN_UNIT};

    #[test]
    fn render_prints_each_place_and_each_reason_on_its_own_line() {
        let response = PlacesResponse {
            working_copy_root: Some("/work/billing".to_string()),
            column_unit: COLUMN_UNIT.to_string(),
            places: vec![
                EntityPlace::Projected {
                    entity_id: "a".to_string(),
                    name: "invoice_total".to_string(),
                    kind: EntityKind::Function,
                    path: "billing/invoice.py".to_string(),
                    start_line: 33,
                    start_column: 1,
                    end_line: 39,
                    end_column: 60,
                },
                EntityPlace::NotProjected {
                    entity_id: "b".to_string(),
                    reason: NotProjectedReason::UnknownEntity,
                    detail: NotProjectedReason::UnknownEntity.detail().to_string(),
                    superseded_by: None,
                },
            ],
        };

        assert_eq!(
            render(&response),
            "invoice_total [function] billing/invoice.py:33:1\n\
             b not projected: The graph holds no entity with this id.\n"
        );
    }
}
