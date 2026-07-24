//! `search` verb: response DTO, corpus-scan policy, and ranking.
//!
//! The `#[tool]` wrapper stays in the parent module's single
//! `#[tool_router(server_handler)]` block (rmcp's macro scans one impl
//! block); this module owns the response DTO, the corpus walk + scoring
//! policy, and the search tests.

use chrono::{DateTime, Utc};
use rmcp::model::ErrorData;
use schemars::JsonSchema;
use serde::Serialize;

use crate::domain::{
    PhaseListFilter, ProjectListFilter, SearchArgs, SearchHit, SearchKind, TaskListFilter,
};

use super::{store_err, MeshService};

/// Response envelope for `search`.
#[derive(Serialize, JsonSchema)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
}

impl MeshService {
    /// Case-insensitive literal substring search across the corpus via
    /// [`Store`] reads. Application-layer query — not a storage verb.
    // `too_many_lines`: single corpus walk; splitting adds indirection
    // without clarity benefit on a structurally linear function.
    // `cast_precision_loss`: `score` is a match count; f64 precision is
    // only lost past 2^53 matches, which is unreachable in practice.
    #[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
    pub(super) async fn search_corpus(
        &self,
        args: &SearchArgs,
    ) -> Result<Vec<SearchHit>, ErrorData> {
        const SNIPPET_CHARS: usize = 80;
        let needle = args.query.trim();
        let limit = args.limit.unwrap_or(50);
        let kinds = args
            .kinds
            .clone()
            .unwrap_or_else(|| vec![SearchKind::Project, SearchKind::Phase, SearchKind::Task]);
        let want_project = kinds.contains(&SearchKind::Project);
        let want_phase = kinds.contains(&SearchKind::Phase);
        let want_task = kinds.contains(&SearchKind::Task);
        // Search defaults to all statuses (finding completed work is core,
        // D4); `include_terminal: false` scopes the walk to live items. This
        // is applied here during the scan — never through the list-verb `From`
        // conversion — so the all-statuses store reads below are unaffected.
        let include_terminal = args.include_terminal.unwrap_or(true);

        let project_slugs: Vec<String> = match &args.project {
            Some(s) if s.is_empty() => {
                return Err(ErrorData::invalid_params(
                    "search: project filter must be non-empty (omit `project` field for corpus-wide search)",
                    None,
                ));
            }
            Some(s) => vec![s.clone()],
            None => self
                .store
                .list_projects(ProjectListFilter::default())
                .await
                .map_err(store_err)?
                .into_iter()
                .map(|v| v.value.slug)
                .collect(),
        };

        let mut ranked: Vec<(SearchHit, DateTime<Utc>)> = Vec::new();

        for proj_slug in &project_slugs {
            if want_project {
                let Ok(p) = self.store.get_project(proj_slug).await else {
                    continue;
                };
                let p = p.value;
                let hay = format!("{}\n{}", p.title, p.description);
                let score = count_ci_overlapping(&hay, needle);
                // Item-level filtering: the project hit is gated inline (not an
                // early `continue`) because a terminal project can still hold
                // live phases/tasks that must be searched in the same iteration.
                let live_kept = include_terminal || !p.status.is_terminal();
                if score > 0 && live_kept {
                    ranked.push((
                        SearchHit {
                            kind: SearchKind::Project,
                            id: p.id.clone(),
                            project: p.slug.clone(),
                            phase: None,
                            slug: p.slug.clone(),
                            title: p.title.clone(),
                            snippet: search_snippet(&hay, needle, SNIPPET_CHARS),
                            score: score as f64,
                        },
                        p.updated_at,
                    ));
                }
            }

            let phase_list = if want_phase || want_task {
                self.store
                    .list_phases(PhaseListFilter {
                        project: Some(proj_slug.clone()),
                        ..Default::default()
                    })
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .map(|v| v.value)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let id_to_phase_slug: std::collections::HashMap<&str, &str> = phase_list
                .iter()
                .map(|ph| (ph.id.as_str(), ph.slug.as_str()))
                .collect();

            if want_phase {
                for ph in &phase_list {
                    if !include_terminal && ph.status.is_terminal() {
                        continue;
                    }
                    let hay = format!("{}\n{}", ph.title, ph.body);
                    let score = count_ci_overlapping(&hay, needle);
                    if score > 0 {
                        ranked.push((
                            SearchHit {
                                kind: SearchKind::Phase,
                                id: ph.id.clone(),
                                project: proj_slug.clone(),
                                phase: None,
                                slug: ph.slug.clone(),
                                title: ph.title.clone(),
                                snippet: search_snippet(&hay, needle, SNIPPET_CHARS),
                                score: score as f64,
                            },
                            ph.updated_at,
                        ));
                    }
                }
            }

            if want_task {
                let tasks = self
                    .store
                    .list_tasks(TaskListFilter {
                        project: Some(proj_slug.clone()),
                        ..Default::default()
                    })
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .map(|v| v.value)
                    .collect::<Vec<_>>();
                for t in tasks {
                    if !include_terminal && t.status.is_terminal() {
                        continue;
                    }
                    let hay = format!("{}\n{}", t.title, t.body);
                    let score = count_ci_overlapping(&hay, needle);
                    if score > 0 {
                        let phase_slug = if t.phase.is_empty() {
                            None
                        } else {
                            id_to_phase_slug
                                .get(t.phase.as_str())
                                .map(|s| (*s).to_owned())
                        };
                        ranked.push((
                            SearchHit {
                                kind: SearchKind::Task,
                                id: t.id.clone(),
                                project: proj_slug.clone(),
                                phase: phase_slug,
                                slug: t.slug.clone(),
                                title: t.title.clone(),
                                snippet: search_snippet(&hay, needle, SNIPPET_CHARS),
                                score: score as f64,
                            },
                            t.updated_at,
                        ));
                    }
                }
            }
        }

        ranked.sort_by(|(ha, ta), (hb, tb)| hb.score.total_cmp(&ha.score).then_with(|| tb.cmp(ta)));

        Ok(ranked.into_iter().take(limit).map(|(h, _)| h).collect())
    }
}

fn unicode_ci_eq(a: char, b: char) -> bool {
    let sa: String = a.to_lowercase().collect();
    let sb: String = b.to_lowercase().collect();
    sa == sb
}

fn find_ci_start_byte(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    'outer: for (start, _) in haystack.char_indices() {
        let tail = &haystack[start..];
        let mut t = tail;
        for nc in needle.chars() {
            let Some(hc) = t.chars().next() else {
                continue 'outer;
            };
            if !unicode_ci_eq(hc, nc) {
                continue 'outer;
            }
            t = &t[hc.len_utf8()..];
        }
        return Some(start);
    }
    None
}

fn count_ci_overlapping(haystack: &str, needle: &str) -> usize {
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();
    if n.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut s = 0usize;
    while let Some(i) = h[s..].find(&n) {
        count += 1;
        let first_char_len = h[s + i..].chars().next().map_or(1, char::len_utf8);
        s += i + first_char_len;
    }
    count
}

fn search_snippet(haystack: &str, needle: &str, width: usize) -> String {
    let Some(start_byte) = find_ci_start_byte(haystack, needle) else {
        return String::new();
    };
    let start_char = haystack[..start_byte].chars().count();
    let chars: Vec<char> = haystack.chars().collect();
    let half = width / 2;
    let lo = start_char.saturating_sub(half);
    let hi = (lo + width).min(chars.len());
    chars.into_iter().skip(lo).take(hi - lo).collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::cast_possible_truncation
    )]

    use super::*;
    use crate::server::task::{TaskCompleteArgs, TaskUpdateArgs};
    use crate::server::testutil::{
        block_on, fresh_service, repo_root, seed_project, seed_store, seed_task, set_task_body,
        set_task_field,
    };
    use crate::store::{FsStore, NewPhase, NewProject, NewTask, UpdateTask};
    use rmcp::handler::server::wrapper::{Json, Parameters};

    fn search_hits(svc: &MeshService, args: SearchArgs) -> Vec<SearchHit> {
        let Json(result) = block_on(svc.search(Parameters(args))).expect("search");
        result.hits
    }

    #[test]
    fn search_rejects_whitespace_only_query() {
        let (_tmp, svc) = fresh_service();
        let args = SearchArgs {
            query: "   ".to_owned(),
            ..Default::default()
        };
        match block_on(svc.search(Parameters(args))) {
            Err(e) => assert!(
                e.message.to_lowercase().contains("query") || e.message.contains("non-empty"),
                "{}",
                e.message
            ),
            Ok(_) => panic!("whitespace-only query must be rejected"),
        }
    }

    #[test]
    fn search_args_rejects_bad_kind() {
        let raw = r#"{"query":"x", "kinds": ["wat"]}"#;
        assert!(
            serde_json::from_str::<SearchArgs>(raw).is_err(),
            "unknown kind must fail deserialization"
        );
    }

    #[test]
    fn dogfood_search_smoke_against_real_corpus() {
        let store = FsStore::open(repo_root()).expect("open corpus");
        let svc = MeshService::new(store);

        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "dossier".to_owned(),
                ..Default::default()
            },
        );
        assert!(!hits.is_empty(), "expected at least one 'dossier' hit");

        let none = search_hits(
            &svc,
            SearchArgs {
                query: "DEFINITELY-NOT-IN-CORPUS-XYZ-9999".to_owned(),
                ..Default::default()
            },
        );
        assert!(none.is_empty());
    }

    #[test]
    fn search_filters_in_temp_corpus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));

        block_on(svc.create_project(NewProject {
            slug: "alpha".to_owned(),
            title: "alpha needle project".to_owned(),
            description: "needle".to_owned(),
            actor: "t".to_owned(),
        }))
        .expect("seed alpha project");
        block_on(svc.create_project(NewProject {
            slug: "beta".to_owned(),
            title: "beta needle project".to_owned(),
            description: "needle".to_owned(),
            actor: "t".to_owned(),
        }))
        .expect("seed beta project");
        block_on(svc.add_phase(&NewPhase {
            project: "alpha".to_owned(),
            slug: "ph".to_owned(),
            title: "phase has needle".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "t".to_owned(),
            owner: "human:test".to_owned(),
        }))
        .expect("seed alpha phase");
        block_on(svc.create_task(NewTask {
            project: "alpha".to_owned(),
            phase: None,
            slug: "ta".to_owned(),
            title: "task has needle".to_owned(),
            body: String::new(),
            actor: "t".to_owned(),
            depends_on: Vec::new(),
        }))
        .expect("seed alpha task");
        block_on(svc.create_task(NewTask {
            project: "beta".to_owned(),
            phase: None,
            slug: "tb".to_owned(),
            title: "task has needle".to_owned(),
            body: String::new(),
            actor: "t".to_owned(),
            depends_on: Vec::new(),
        }))
        .expect("seed beta task");

        let all = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                ..Default::default()
            },
        );
        let projects: std::collections::HashSet<_> =
            all.iter().map(|h| h.project.clone()).collect();
        assert_eq!(projects.len(), 2);

        let tasks = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                kinds: Some(vec![SearchKind::Task]),
                ..Default::default()
            },
        );
        assert!(!tasks.is_empty());
        assert!(tasks.iter().all(|h| h.kind == SearchKind::Task));

        let alpha_only = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                project: Some("alpha".to_owned()),
                ..Default::default()
            },
        );
        assert!(!alpha_only.is_empty());
        assert!(alpha_only.iter().all(|h| h.project == "alpha"));

        let alpha_tasks = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                project: Some("alpha".to_owned()),
                kinds: Some(vec![SearchKind::Task]),
                ..Default::default()
            },
        );
        assert!(!alpha_tasks.is_empty());
        assert!(alpha_tasks
            .iter()
            .all(|h| h.project == "alpha" && h.kind == SearchKind::Task));
    }

    #[test]
    fn search_title_and_body_hits_in_temp_corpus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));

        block_on(svc.create_project(NewProject {
            slug: "p1".to_owned(),
            title: "TITLEKEY-alpha project".to_owned(),
            description: "minimal".to_owned(),
            actor: "t".to_owned(),
        }))
        .expect("seed project");
        block_on(svc.add_phase(&NewPhase {
            project: "p1".to_owned(),
            slug: "ph1".to_owned(),
            title: "TITLEKEY-beta phase".to_owned(),
            body: "body has BODYKEY-gamma token".to_owned(),
            after_phase: None,
            actor: "t".to_owned(),
            owner: "human:test".to_owned(),
        }))
        .expect("seed phase");
        block_on(svc.create_task(NewTask {
            project: "p1".to_owned(),
            phase: Some("ph1".to_owned()),
            slug: "tsk".to_owned(),
            title: "TITLEKEY-delta task".to_owned(),
            body: "BODYKEY-epsilon in spec".to_owned(),
            actor: "t".to_owned(),
            depends_on: Vec::new(),
        }))
        .expect("seed task");

        let t_alpha = search_hits(
            &svc,
            SearchArgs {
                query: "titlekey-alpha".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_alpha.len(), 1);
        assert_eq!(t_alpha[0].kind, SearchKind::Project);

        let t_beta = search_hits(
            &svc,
            SearchArgs {
                query: "titlekey-beta".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_beta.len(), 1);
        assert_eq!(t_beta[0].kind, SearchKind::Phase);

        let t_gamma = search_hits(
            &svc,
            SearchArgs {
                query: "bodykey-gamma".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_gamma.len(), 1);
        assert_eq!(t_gamma[0].kind, SearchKind::Phase);

        let t_delta = search_hits(
            &svc,
            SearchArgs {
                query: "titlekey-delta".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_delta.len(), 1);
        assert_eq!(t_delta[0].kind, SearchKind::Task);

        let t_eps = search_hits(
            &svc,
            SearchArgs {
                query: "bodykey-epsilon".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_eps.len(), 1);
        assert_eq!(t_eps[0].kind, SearchKind::Task);

        let uni = search_hits(
            &svc,
            SearchArgs {
                query: "KEY-".to_owned(),
                ..Default::default()
            },
        );
        let kinds: std::collections::HashSet<_> = uni.iter().map(|h| h.kind).collect();
        assert_eq!(kinds.len(), 3);
    }

    #[test]
    fn search_ranks_higher_score_before_lower() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        let a = seed_task(&svc, "alpha", "one-match");
        let b = seed_task(&svc, "alpha", "triple");
        set_task_body(tmp.path(), &a, "needleonce");
        set_task_body(tmp.path(), &b, "needleneedleneedle");
        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                project: Some("alpha".to_owned()),
                kinds: Some(vec![SearchKind::Task]),
                ..Default::default()
            },
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].slug, "triple");
        assert_eq!(hits[0].score as i64, 3);
        assert_eq!(hits[1].score as i64, 1);
    }

    #[test]
    fn search_tiebreaks_by_updated_at_desc() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        let a = seed_task(&svc, "alpha", "oldhit");
        let b = seed_task(&svc, "alpha", "newhit");
        set_task_body(tmp.path(), &a, "sameneedle");
        set_task_body(tmp.path(), &b, "sameneedle");
        set_task_field(tmp.path(), &a, "updated_at", "2026-01-01T00:00:00Z");
        set_task_field(tmp.path(), &b, "updated_at", "2026-06-01T00:00:00Z");
        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "sameneedle".to_owned(),
                project: Some("alpha".to_owned()),
                kinds: Some(vec![SearchKind::Task]),
                ..Default::default()
            },
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].slug, "newhit");
        assert_eq!(hits[1].slug, "oldhit");
    }

    #[test]
    fn search_limit_after_sort() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        for i in 0..5 {
            let slug = format!("t{i}");
            let task = seed_task(&svc, "alpha", &slug);
            set_task_body(tmp.path(), &task, &format!("{} needle", "x".repeat(i + 1)));
            set_task_field(
                tmp.path(),
                &task,
                "updated_at",
                &format!("2026-05-{:02}T00:00:00Z", 10 + i),
            );
        }
        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                project: Some("alpha".to_owned()),
                kinds: Some(vec![SearchKind::Task]),
                include_terminal: None,
                limit: Some(2),
            },
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].slug, "t4");
        assert_eq!(hits[1].slug, "t3");
    }

    #[test]
    fn search_notes_section_not_indexed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        let task = seed_task(&svc, "alpha", "n");
        set_task_body(tmp.path(), &task, "spec has no secret");
        block_on(svc.update_task(UpdateTask {
            id: task.id,
            body: None,
            status: None,
            note: Some("note about zzzuniquezzz term".to_owned()),
            actor: "t".to_owned(),
            depends_on: None,
        }))
        .expect("append note");
        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "zzzuniquezzz".to_owned(),
                project: Some("alpha".to_owned()),
                ..Default::default()
            },
        );
        assert!(
            hits.is_empty(),
            "notes must not be searchable, got {hits:?}"
        );
    }

    fn complete_task(svc: &MeshService, id: &str) {
        block_on(svc.task_complete(Parameters(TaskCompleteArgs {
            id: id.to_owned(),
            note: None,
            actor: "human:test".to_owned(),
        })))
        .expect("complete task");
    }

    #[test]
    fn search_includes_terminal_by_default_and_scopes_with_flag() {
        let (_tmp, svc) = fresh_service();
        seed_project(&svc, "alpha");
        let live = seed_task(&svc, "alpha", "live-needle");
        let term = seed_task(&svc, "alpha", "terminal-needle");
        // Distinct bodies so the same query matches both.
        block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: live.id,
            body: Some("magicword lives".to_owned()),
            status: None,
            note: None,
            depends_on: None,
            actor: "human:test".to_owned(),
        })))
        .expect("body live");
        block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: term.id.clone(),
            body: Some("magicword done".to_owned()),
            status: None,
            note: None,
            depends_on: None,
            actor: "human:test".to_owned(),
        })))
        .expect("body term");
        complete_task(&svc, &term.id);

        let default_hits = search_hits(
            &svc,
            SearchArgs {
                query: "magicword".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(
            default_hits.len(),
            2,
            "search includes terminal hits by default (D4)"
        );

        let live_only = search_hits(
            &svc,
            SearchArgs {
                query: "magicword".to_owned(),
                include_terminal: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(live_only.len(), 1, "include_terminal:false scopes to live");
        assert_eq!(live_only[0].slug, "live-needle");
    }
}
