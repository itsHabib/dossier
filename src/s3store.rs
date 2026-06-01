//! S3-backed [`Store`] using conditional PUTs for compare-and-swap writes.

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use aws_smithy_runtime_api::client::result::SdkError;
use aws_smithy_runtime_api::http::Response;
use futures::stream::{self, StreamExt};
use futures::TryStreamExt;

use crate::domain::{
    is_valid_slug, Artifact, Phase, PhaseListFilter, Project, ProjectListFilter, Task,
    TaskListFilter,
};
use crate::store::{
    notes_lines_for_task, parse_phase, parse_project, parse_task, phase_filename, phase_matches,
    project_matches, serialize_phase_file, serialize_project_file, serialize_task_file,
    sort_phases, sort_projects, sort_tasks, task_filename, task_matches, ArtifactListFilter, Store,
    StoreError, Version, Versioned,
};

const LIST_CONCURRENCY: usize = 16;
const ARTIFACT_PUT_RETRIES: usize = 5;

fn key_is_markdown(key: &str) -> bool {
    std::path::Path::new(key)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Connection settings for [`S3Store`]. Does not create the bucket.
#[derive(Debug, Clone)]
pub struct S3Config {
    pub bucket: String,
    /// Tenant segment; empty means bucket root. No leading or trailing `/`.
    pub prefix: String,
    /// `Some("http://localhost:9000")` for `MinIO`; `None` for real AWS.
    pub endpoint_url: Option<String>,
    /// `MinIO` ignores this; the SDK requires a region. Default `us-east-1`.
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Set `true` for `MinIO` path-style URLs.
    pub force_path_style: bool,
}

/// Object store backend mirroring the on-disk corpus layout under an optional prefix.
#[derive(Debug)]
pub struct S3Store {
    client: Client,
    bucket: String,
    prefix: String,
}

struct ObjectBody {
    raw: String,
    version: Version,
}

impl S3Store {
    /// Build an S3 client from `cfg`. Does not create the bucket.
    pub async fn new(cfg: S3Config) -> Result<Self, StoreError> {
        let creds = Credentials::new(
            cfg.access_key_id,
            cfg.secret_access_key,
            None,
            None,
            "dossier-s3store",
        );
        let shared = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_config::Region::new(cfg.region))
            .credentials_provider(creds)
            .load()
            .await;
        let mut s3_builder = aws_sdk_s3::config::Builder::from(&shared);
        if let Some(url) = &cfg.endpoint_url {
            s3_builder = s3_builder.endpoint_url(url);
        }
        s3_builder = s3_builder.force_path_style(cfg.force_path_style);
        let client = Client::from_conf(s3_builder.build());
        Ok(Self {
            client,
            bucket: cfg.bucket,
            prefix: Self::normalize_prefix(&cfg.prefix)?,
        })
    }

    fn normalize_prefix(prefix: &str) -> Result<String, StoreError> {
        if prefix.starts_with('/') || prefix.ends_with('/') {
            return Err(StoreError::Invalid(
                "prefix must not have leading or trailing '/'".into(),
            ));
        }
        Ok(prefix.to_owned())
    }

    fn join_key(&self, segments: &[&str]) -> String {
        let mut out = String::new();
        if !self.prefix.is_empty() {
            out.push_str(&self.prefix);
            out.push('/');
        }
        out.push_str(&segments.join("/"));
        out
    }

    fn validate_slug(slug: &str) -> Result<(), StoreError> {
        if is_valid_slug(slug) {
            Ok(())
        } else {
            Err(StoreError::Invalid(format!("invalid slug: {slug}")))
        }
    }

    async fn list_object_keys(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req.send().await.map_err(map_sdk_err)?;
            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    keys.push(key.to_owned());
                }
            }
            if resp.is_truncated() == Some(true) {
                token = resp.next_continuation_token().map(str::to_owned);
            } else {
                break;
            }
        }
        Ok(keys)
    }

    async fn list_project_slugs(&self) -> Result<Vec<String>, StoreError> {
        let list_prefix = format!("{}/", self.join_key(&["projects"]));
        let mut slugs = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&list_prefix)
                .delimiter("/");
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req.send().await.map_err(map_sdk_err)?;
            for cp in resp.common_prefixes() {
                if let Some(p) = cp.prefix() {
                    if let Some(slug) = project_slug_from_prefix(p, &list_prefix) {
                        slugs.push(slug);
                    }
                }
            }
            if resp.is_truncated() == Some(true) {
                token = resp.next_continuation_token().map(str::to_owned);
            } else {
                break;
            }
        }
        slugs.sort();
        Ok(slugs)
    }

    async fn get_object_body(&self, key: &str) -> Result<ObjectBody, StoreError> {
        self.get_object_body_optional(key)
            .await?
            .ok_or(StoreError::NotFound)
    }

    async fn get_object_body_optional(&self, key: &str) -> Result<Option<ObjectBody>, StoreError> {
        let resp = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) if is_not_found(&err) => return Ok(None),
            Err(err) => return Err(map_sdk_err(err)),
        };
        let version = etag_from_output(resp.e_tag())?;
        let raw = body_to_string(resp.body).await?;
        Ok(Some(ObjectBody { raw, version }))
    }

    async fn cas_put(
        &self,
        key: &str,
        content: &[u8],
        expected: Option<&Version>,
    ) -> Result<Version, StoreError> {
        if let Some(expected_v) = expected {
            match self.get_object_body_optional(key).await? {
                None => return Err(StoreError::NotFound),
                Some(obj) if obj.version.as_str() != expected_v.as_str() => {
                    return Err(StoreError::Conflict);
                }
                Some(_) => {}
            }
        }
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(content.to_vec()));
        req = match expected {
            None => req.if_none_match("*"),
            Some(v) => req.if_match(v.as_str()),
        };
        let resp = req.send().await.map_err(map_sdk_err)?;
        etag_from_output(resp.e_tag())
    }

    async fn delete_object(&self, key: &str) -> Result<(), StoreError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }

    async fn load_phases_for(&self, project_slug: &str) -> Result<Vec<Phase>, StoreError> {
        Self::validate_slug(project_slug)?;
        let prefix = format!("{}/", self.join_key(&["projects", project_slug, "phases"]));
        let keys = self.list_object_keys(&prefix).await?;
        let mut out = Vec::new();
        for key in keys {
            if !key_is_markdown(&key) {
                continue;
            }
            let obj = self.get_object_body(&key).await?;
            let phase = parse_phase(&obj.raw).map_err(invalid_err)?;
            out.push(phase);
        }
        Ok(out)
    }

    async fn load_tasks_for(&self, project_slug: &str) -> Result<Vec<Task>, StoreError> {
        Self::validate_slug(project_slug)?;
        let prefix = format!("{}/", self.join_key(&["projects", project_slug, "tasks"]));
        let keys = self.list_object_keys(&prefix).await?;
        let mut out = Vec::new();
        for key in keys {
            if !key_is_markdown(&key) {
                continue;
            }
            let obj = self.get_object_body(&key).await?;
            let (task, _notes) = parse_task(&obj.raw, project_slug).map_err(invalid_err)?;
            out.push(task);
        }
        Ok(out)
    }

    async fn find_phase_key(
        &self,
        project_slug: &str,
        phase_slug: &str,
    ) -> Result<String, StoreError> {
        Self::validate_slug(project_slug)?;
        let prefix = format!("{}/", self.join_key(&["projects", project_slug, "phases"]));
        let keys = self.list_object_keys(&prefix).await?;
        for key in keys {
            let Some(name) = key.rsplit('/').next() else {
                continue;
            };
            if !key_is_markdown(name) {
                continue;
            }
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            if let Some((_, slug)) = stem.split_once('-') {
                if slug == phase_slug {
                    return Ok(key);
                }
            }
        }
        Err(StoreError::NotFound)
    }

    async fn find_task_key(&self, task_id: &str) -> Result<(String, String), StoreError> {
        let mut found: Option<(String, String)> = None;
        for project_slug in self.list_project_slugs().await? {
            let prefix = format!("{}/", self.join_key(&["projects", &project_slug, "tasks"]));
            let keys = self.list_object_keys(&prefix).await?;
            for key in keys {
                let Some(name) = key.rsplit('/').next() else {
                    continue;
                };
                if !key_is_markdown(name) {
                    continue;
                }
                let Some(stem) = name.strip_suffix(".md") else {
                    continue;
                };
                if let Some((id, _slug)) = stem.split_once('-') {
                    if id == task_id {
                        if found.is_some() {
                            return Err(StoreError::Invalid(format!(
                                "duplicate task id: {task_id}"
                            )));
                        }
                        found = Some((project_slug.clone(), key));
                    }
                }
            }
        }
        found.ok_or(StoreError::NotFound)
    }

    async fn project_slug_for_id(&self, project_id: &str) -> Result<String, StoreError> {
        for slug in self.list_project_slugs().await? {
            let key = self.join_key(&["projects", &slug, "project.md"]);
            let Some(obj) = self.get_object_body_optional(&key).await? else {
                continue;
            };
            let project = parse_project(&obj.raw, false).map_err(invalid_err)?;
            if project.id == project_id {
                return Ok(slug);
            }
        }
        Err(StoreError::NotFound)
    }

    async fn project_slug_for_phase_id(&self, phase_id: &str) -> Result<String, StoreError> {
        for slug in self.list_project_slugs().await? {
            let phases = self.load_phases_for(&slug).await?;
            if phases.iter().any(|p| p.id == phase_id) {
                return Ok(slug);
            }
        }
        Err(StoreError::NotFound)
    }

    async fn read_artifacts(&self, project_slug: &str) -> Result<Vec<Artifact>, StoreError> {
        Self::validate_slug(project_slug)?;
        let key = self.join_key(&["projects", project_slug, "artifacts.jsonl"]);
        match self.get_object_body_optional(&key).await? {
            None => Ok(Vec::new()),
            Some(obj) => parse_artifacts_jsonl(&obj.raw),
        }
    }

    async fn get_versioned_projects(
        &self,
        slugs: &[String],
        with_body: bool,
    ) -> Result<Vec<Versioned<Project>>, StoreError> {
        stream::iter(slugs.iter().cloned())
            .map(|slug| async move {
                let key = self.join_key(&["projects", &slug, "project.md"]);
                let obj = self.get_object_body(&key).await?;
                let project = parse_project(&obj.raw, with_body).map_err(invalid_err)?;
                Ok::<_, StoreError>(Versioned {
                    value: project,
                    version: obj.version,
                })
            })
            .buffer_unordered(LIST_CONCURRENCY)
            .try_collect()
            .await
    }
}

#[async_trait]
impl Store for S3Store {
    async fn get_project(&self, slug: &str) -> Result<Versioned<Project>, StoreError> {
        Self::validate_slug(slug)?;
        let key = self.join_key(&["projects", slug, "project.md"]);
        let obj = self.get_object_body(&key).await?;
        let project = parse_project(&obj.raw, true).map_err(invalid_err)?;
        Ok(Versioned {
            value: project,
            version: obj.version,
        })
    }

    async fn list_projects(
        &self,
        filter: ProjectListFilter,
    ) -> Result<Vec<Versioned<Project>>, StoreError> {
        let slugs = self.list_project_slugs().await?;
        let needs_body = filter.body_contains.is_some();
        let versioned = self.get_versioned_projects(&slugs, needs_body).await?;
        let mut projects: Vec<Project> = versioned.iter().map(|v| v.value.clone()).collect();
        projects.retain(|p| project_matches(p, &filter));
        sort_projects(&mut projects, &filter);
        if let Some(limit) = filter.limit {
            projects.truncate(limit);
        }
        for p in &mut projects {
            p.description.clear();
        }
        let mut out = Vec::with_capacity(projects.len());
        for p in projects {
            let version = versioned
                .iter()
                .find(|v| v.value.id == p.id)
                .map(|v| v.version.clone())
                .ok_or(StoreError::NotFound)?;
            out.push(Versioned { value: p, version });
        }
        Ok(out)
    }

    async fn get_phase(&self, project: &str, slug: &str) -> Result<Versioned<Phase>, StoreError> {
        let key = self.find_phase_key(project, slug).await?;
        let obj = self.get_object_body(&key).await?;
        let phase = parse_phase(&obj.raw).map_err(invalid_err)?;
        Ok(Versioned {
            value: phase,
            version: obj.version,
        })
    }

    async fn list_phases(
        &self,
        filter: PhaseListFilter,
    ) -> Result<Vec<Versioned<Phase>>, StoreError> {
        let slugs = if let Some(project) = &filter.project {
            Self::validate_slug(project)?;
            vec![project.clone()]
        } else {
            self.list_project_slugs().await?
        };
        let mut phases = Vec::new();
        for project_slug in slugs {
            phases.extend(self.load_phases_for(&project_slug).await?);
        }
        phases.retain(|p| phase_matches(p, &filter));
        sort_phases(&mut phases, &filter);
        if let Some(limit) = filter.limit {
            phases.truncate(limit);
        }
        let project_filter = filter.project.clone();
        stream::iter(phases)
            .map(move |phase| {
                let project_filter = project_filter.clone();
                async move {
                    let project_slug = if let Some(p) = &project_filter {
                        p.clone()
                    } else {
                        self.project_slug_for_phase_id(&phase.id).await?
                    };
                    let key = self.find_phase_key(&project_slug, &phase.slug).await?;
                    let obj = self.get_object_body(&key).await?;
                    Ok(Versioned {
                        value: phase,
                        version: obj.version,
                    })
                }
            })
            // buffered (not buffer_unordered) so results stay in the sorted order
            // established above — buffer_unordered would yield in completion order.
            .buffered(LIST_CONCURRENCY)
            .try_collect()
            .await
    }

    async fn get_task(&self, id: &str) -> Result<Versioned<Task>, StoreError> {
        let (project_slug, key) = self.find_task_key(id).await?;
        let obj = self.get_object_body(&key).await?;
        let (task, _notes) = parse_task(&obj.raw, &project_slug).map_err(invalid_err)?;
        Ok(Versioned {
            value: task,
            version: obj.version,
        })
    }

    async fn list_tasks(&self, filter: TaskListFilter) -> Result<Vec<Versioned<Task>>, StoreError> {
        let resolved_phase_id: Option<String> = match (&filter.project, &filter.phase) {
            (Some(project_slug), Some(phase_slug)) => {
                let phases = self.load_phases_for(project_slug).await?;
                let phase = phases
                    .iter()
                    .find(|p| &p.slug == phase_slug)
                    .ok_or_else(|| {
                        StoreError::Invalid(format!(
                            "phase not found: {phase_slug} in project {project_slug}"
                        ))
                    })?;
                Some(phase.id.clone())
            }
            _ => None,
        };

        let mut tasks = if let Some(slug) = &filter.project {
            self.load_tasks_for(slug).await?
        } else {
            let mut all = Vec::new();
            for slug in self.list_project_slugs().await? {
                all.extend(self.load_tasks_for(&slug).await?);
            }
            all
        };
        tasks.retain(|t| task_matches(t, &filter, resolved_phase_id.as_deref()));
        sort_tasks(&mut tasks, &filter);
        if let Some(limit) = filter.limit {
            tasks.truncate(limit);
        }
        stream::iter(tasks)
            .map(|task| async move {
                let key = self.join_key(&[
                    "projects",
                    &task.project_slug,
                    "tasks",
                    &task_filename(&task.id, &task.slug),
                ]);
                let obj = self.get_object_body(&key).await?;
                Ok(Versioned {
                    value: task,
                    version: obj.version,
                })
            })
            // buffered (not buffer_unordered) so results stay in the sorted order
            // established above — buffer_unordered would yield in completion order.
            .buffered(LIST_CONCURRENCY)
            .try_collect()
            .await
    }

    async fn list_artifacts(
        &self,
        filter: ArtifactListFilter,
    ) -> Result<Vec<Artifact>, StoreError> {
        self.read_artifacts(&filter.project).await
    }

    async fn put_project(
        &self,
        project: &Project,
        expected: Option<Version>,
    ) -> Result<Version, StoreError> {
        Self::validate_slug(&project.slug)?;
        let key = self.join_key(&["projects", &project.slug, "project.md"]);
        let content = serialize_project_file(project).map_err(invalid_err)?;
        self.cas_put(&key, content.as_bytes(), expected.as_ref())
            .await
    }

    async fn put_phase(
        &self,
        phase: &Phase,
        expected: Option<Version>,
    ) -> Result<Version, StoreError> {
        let project_slug = self.project_slug_for_id(&phase.project).await?;
        let content = serialize_phase_file(phase).map_err(invalid_err)?;
        let key = if expected.is_none() {
            self.join_key(&[
                "projects",
                &project_slug,
                "phases",
                &phase_filename(phase.order, &phase.slug),
            ])
        } else {
            self.find_phase_key(&project_slug, &phase.slug).await?
        };
        self.cas_put(&key, content.as_bytes(), expected.as_ref())
            .await
    }

    async fn put_task(
        &self,
        task: &Task,
        expected: Option<Version>,
    ) -> Result<Version, StoreError> {
        Self::validate_slug(&task.project_slug)?;
        Self::validate_slug(&task.slug)?;
        let key = self.join_key(&[
            "projects",
            &task.project_slug,
            "tasks",
            &task_filename(&task.id, &task.slug),
        ]);
        let notes_lines = notes_lines_for_task(task);
        let content = serialize_task_file(task, &notes_lines).map_err(invalid_err)?;
        self.cas_put(&key, content.as_bytes(), expected.as_ref())
            .await
    }

    async fn put_artifact(&self, artifact: &Artifact) -> Result<(), StoreError> {
        let project_slug = self.project_slug_for_id(&artifact.project).await?;
        let key = self.join_key(&["projects", &project_slug, "artifacts.jsonl"]);
        let line =
            serde_json::to_string(artifact).map_err(|e| StoreError::Invalid(e.to_string()))?;
        for _ in 0..ARTIFACT_PUT_RETRIES {
            let existing = self.read_artifacts(&project_slug).await?;
            if existing.iter().any(|a| a.id == artifact.id) {
                return Err(StoreError::Conflict);
            }
            let current = self.get_object_body_optional(&key).await?;
            let mut body = current
                .as_ref()
                .map_or_else(String::new, |obj| obj.raw.clone());
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&line);
            body.push('\n');
            let expected = current.as_ref().map(|obj| &obj.version);
            match self.cas_put(&key, body.as_bytes(), expected).await {
                Ok(_) => return Ok(()),
                Err(StoreError::Conflict) => {}
                Err(err) => return Err(err),
            }
        }
        Err(StoreError::Conflict)
    }

    async fn shift_phases(&self, project: &str, from_order: i32) -> Result<(), StoreError> {
        Self::validate_slug(project)?;
        let mut phases = self.load_phases_for(project).await?;
        phases.sort_by_key(|p| std::cmp::Reverse(p.order));
        for phase in &mut phases {
            if phase.order < from_order {
                continue;
            }
            let old_key = self.find_phase_key(project, &phase.slug).await?;
            phase.order += 1;
            let content = serialize_phase_file(phase).map_err(invalid_err)?;
            let new_key = self.join_key(&[
                "projects",
                project,
                "phases",
                &phase_filename(phase.order, &phase.slug),
            ]);
            self.cas_put(&new_key, content.as_bytes(), None).await?;
            self.delete_object(&old_key).await?;
        }
        Ok(())
    }
}

fn project_slug_from_prefix(common_prefix: &str, list_prefix: &str) -> Option<String> {
    let rest = common_prefix.strip_prefix(list_prefix)?;
    let slug = rest.strip_suffix('/')?;
    is_valid_slug(slug).then(|| slug.to_owned())
}

fn etag_from_output(etag: Option<&str>) -> Result<Version, StoreError> {
    etag.map(|t| Version::new(t.to_owned()))
        .ok_or_else(|| StoreError::Invalid("missing ETag".into()))
}

async fn body_to_string(body: ByteStream) -> Result<String, StoreError> {
    let bytes = body
        .collect()
        .await
        .map_err(|_| StoreError::Unavailable)?
        .into_bytes();
    String::from_utf8(bytes.to_vec()).map_err(|e| StoreError::Invalid(e.to_string()))
}

fn parse_artifacts_jsonl(raw: &str) -> Result<Vec<Artifact>, StoreError> {
    let mut out = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let artifact: Artifact = serde_json::from_str(line).map_err(|e| {
            StoreError::Invalid(format!("parse artifacts.jsonl line {}: {e}", idx + 1))
        })?;
        out.push(artifact);
    }
    Ok(out)
}

#[allow(clippy::needless_pass_by_value)] // map_err hands errors by value
fn invalid_err(err: anyhow::Error) -> StoreError {
    StoreError::Invalid(err.to_string())
}

fn is_not_found<E>(err: &SdkError<E, Response>) -> bool {
    status_code(err) == Some(404)
}

fn status_code<E>(err: &SdkError<E, Response>) -> Option<u16> {
    err.raw_response().map(|r| r.status().as_u16())
}

#[allow(clippy::needless_pass_by_value)] // map_err hands errors by value
fn map_sdk_err<E: std::fmt::Debug>(err: SdkError<E, Response>) -> StoreError {
    if let Some(code) = status_code(&err) {
        return match code {
            412 => StoreError::Conflict,
            404 => StoreError::NotFound,
            500..=599 => StoreError::Unavailable,
            _ => StoreError::Invalid(format!("{err:?}")),
        };
    }
    StoreError::Unavailable
}
