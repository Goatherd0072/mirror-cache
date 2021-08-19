use crate::cache::{CacheData, CachePolicy, NoCache};
use crate::error::Error;
use crate::error::Result;
use crate::metric;
use crate::settings::Settings;
use crate::util;
use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use metrics::{histogram, increment_counter};
use std::collections::HashMap;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type SharedTaskManager = Arc<TaskManager>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Task {
    PypiIndexTask { pkg_name: String },
    PypiPackagesTask { pkg_path: String },
    AnacondaTask { path: String },
    Others { rule_id: RuleId, url: String },
}

pub enum TaskResponse {
    StringResponse(String),
    BytesResponse(Bytes),
    StreamResponse(Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + Sync>>),
}

impl From<String> for TaskResponse {
    fn from(s: String) -> TaskResponse {
        TaskResponse::StringResponse(s)
    }
}

impl From<CacheData> for TaskResponse {
    fn from(cache_data: CacheData) -> TaskResponse {
        match cache_data {
            CacheData::TextData(text) => text.into(),
            CacheData::BytesData(bytes) => TaskResponse::BytesResponse(bytes),
            CacheData::ByteStream(stream) => TaskResponse::StreamResponse(Box::pin(stream)),
        }
    }
}

impl warp::Reply for TaskResponse {
    fn into_response(self) -> warp::reply::Response {
        match self {
            TaskResponse::StringResponse(content) => warp::reply::Response::new(content.into()),
            TaskResponse::BytesResponse(bytes) => warp::reply::Response::new(bytes.into()),
            TaskResponse::StreamResponse(stream) => {
                warp::reply::Response::new(warp::hyper::Body::wrap_stream(stream))
            }
        }
    }
}

impl Task {
    pub async fn resolve(&self, tm: Arc<TaskManager>) -> Result<TaskResponse> {
        // try get from cache
        let mut cache_result = None;
        let key = self.to_key();
        match &self {
            Task::PypiIndexTask { .. } => {
                if let Some(bytes) = tm.get(&self, &key).await {
                    increment_counter!(metric::CNT_PYPI_INDEX_CACHE_HIT);
                    cache_result = Some(bytes)
                } else {
                    increment_counter!(metric::CNT_PYPI_INDEX_CACHE_MISS);
                }
            }
            Task::PypiPackagesTask { .. } => {
                if let Some(bytes) = tm.get(&self, &key).await {
                    increment_counter!(metric::CNT_PYPI_PKGS_CACHE_HIT);
                    cache_result = Some(bytes)
                } else {
                    increment_counter!(metric::CNT_PYPI_PKGS_CACHE_MISS);
                }
            }
            Task::AnacondaTask { .. } => {
                if let Some(bytes) = tm.get(&self, &key).await {
                    increment_counter!(metric::CNT_ANACONDA_CACHE_HIT);
                    cache_result = Some(bytes)
                } else {
                    increment_counter!(metric::CNT_ANACONDA_CACHE_MISS);
                }
            }
            Task::Others { .. } => {
                if let Some(bytes) = tm.get(&self, &key).await {
                    cache_result = Some(bytes);
                }
            }
        };
        if let Some(data) = cache_result {
            info!("[Request] [HIT] {:?}", &self);
            increment_counter!(metric::COUNTER_CACHE_HIT);
            return Ok(data.into());
        }
        // cache miss, dispatch async cache task
        increment_counter!(metric::COUNTER_CACHE_MISS);
        let _ = tm.spawn_task(self.clone()).await;
        // fetch from upstream
        let remote_url = tm.resolve_task_upstream(&self);
        info!(
            "[Request] [MISS] {:?}, fetching from upstream: {}",
            &self, &remote_url
        );
        let resp = util::make_request(&remote_url).await;
        match resp {
            Ok(res) => match &self {
                Task::PypiIndexTask { .. } => {
                    let text_content = res.text().await.unwrap();
                    if let Some(url) = tm.config.url.clone() {
                        Ok(self.rewrite_upstream(text_content, &url).into())
                    } else {
                        Ok(text_content.into())
                    }
                }
                _ => Ok(TaskResponse::StreamResponse(Box::pin(
                    res.bytes_stream()
                        .map(move |x| x.map_err(|e| Error::RequestError(e))),
                ))),
            },
            Err(e) => {
                error!("[Request] {:?} failed to fetch upstream: {}", &self, e);
                Err(e)
            }
        }
    }

    pub fn rewrite_upstream(&self, input: String, to: &str) -> String {
        match &self {
            Task::PypiIndexTask { .. } => util::pypi_index_rewrite(&input, to),
            _ => input,
        }
    }

    pub fn to_key(&self) -> String {
        match &self {
            Task::PypiIndexTask { pkg_name, .. } => format!("pypi_index_{}", pkg_name),
            Task::PypiPackagesTask { pkg_path, .. } => String::from(pkg_path),
            Task::AnacondaTask { path, .. } => format!("anaconda_{}", path),
            Task::Others { url, .. } => url
                .replace("http://", "http/")
                .replace("https://", "https/"),
        }
    }
}

pub type RuleId = usize;

pub struct TaskManager {
    pub config: Settings,
    pub pypi_index_cache: Arc<dyn CachePolicy>,
    pub pypi_pkg_cache: Arc<dyn CachePolicy>,
    pub anaconda_cache: Arc<dyn CachePolicy>,
    pub cache_map: HashMap<RuleId, Arc<dyn CachePolicy>>,
    task_set: Arc<RwLock<HashSet<Task>>>,
}

impl TaskManager {
    pub fn new(config: Settings) -> Self {
        TaskManager {
            config,
            pypi_index_cache: Arc::new(NoCache {}),
            pypi_pkg_cache: Arc::new(NoCache {}),
            anaconda_cache: Arc::new(NoCache {}),
            cache_map: HashMap::new(),
            task_set: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    async fn taskset_contains(&self, t: &Task) -> bool {
        self.task_set.read().await.contains(t)
    }

    async fn taskset_add(&self, t: Task) {
        self.task_set.write().await.insert(t);
    }

    async fn taskset_remove(task_set: Arc<RwLock<HashSet<Task>>>, t: &Task) {
        task_set.write().await.remove(t);
    }

    async fn taskset_len(task_set: Arc<RwLock<HashSet<Task>>>) -> usize {
        let len = task_set.read().await.len();
        histogram!(metric::HG_TASKS_LEN, len as f64);
        len
    }

    /// Spawn an async task
    async fn spawn_task(&self, task: Task) {
        increment_counter!(metric::COUNTER_TASKS_BG);
        if self.taskset_contains(&task).await {
            info!("[TASK] ignored existing task: {:?}", task);
            return;
        }
        self.taskset_add(task.clone()).await;
        let task_set_len = Self::taskset_len(self.task_set.clone()).await;
        info!("[TASK] [len={}] + {:?}", task_set_len, task);
        let c;
        let mut rewrite = false;
        let mut to_url = None;
        match &task {
            Task::PypiIndexTask { .. } => {
                c = self.pypi_index_cache.clone();
                to_url = self.config.url.clone();
                rewrite = true;
            }
            Task::PypiPackagesTask { .. } => {
                c = self.pypi_pkg_cache.clone();
            }
            Task::AnacondaTask { .. } => {
                c = self.anaconda_cache.clone();
            }
            Task::Others { rule_id, .. } => {
                c = self.get_cache_for_cache_rule(*rule_id).unwrap();
            }
        };
        let task_clone = task.clone();
        let upstream_url = self.resolve_task_upstream(&task_clone);
        let task_list_ptr = self.task_set.clone();
        // spawn an async download task
        tokio::spawn(async move {
            let resp = util::make_request(&upstream_url).await;
            match resp {
                Ok(res) => {
                    if rewrite {
                        let content = res.text().await.ok();
                        if content.is_none() {
                            increment_counter!(metric::CNT_TASKS_BG_FAILURE);
                            return;
                        }
                        let mut content = content.unwrap();
                        if let Some(to_url) = to_url {
                            content = task_clone.rewrite_upstream(content, &to_url);
                        };
                        c.put(&task_clone.to_key(), content.into()).await;
                    } else {
                        let bytestream = res.bytes_stream();
                        c.put(
                            &task_clone.to_key(),
                            CacheData::ByteStream(Box::new(
                                bytestream.map(move |x| x.map_err(|e| Error::RequestError(e))),
                            )),
                        )
                        .await;
                    }
                    increment_counter!(metric::CNT_TASKS_BG_SUCCESS);
                }
                Err(e) => {
                    increment_counter!(metric::CNT_TASKS_BG_FAILURE);
                    error!("[TASK] ❌ failed to fetch upstream: {}", e);
                }
            };
            Self::taskset_remove(task_list_ptr.clone(), &task_clone).await;
            Self::taskset_len(task_list_ptr).await;
        });
    }

    /// get task result from cache
    pub async fn get(&self, task_type: &Task, key: &str) -> Option<CacheData> {
        match &task_type {
            Task::PypiIndexTask { .. } => self.pypi_index_cache.get(key).await,
            Task::PypiPackagesTask { .. } => self.pypi_pkg_cache.get(key).await,
            Task::AnacondaTask { .. } => self.anaconda_cache.get(key).await,
            Task::Others { rule_id, .. } => match self.get_cache_for_cache_rule(*rule_id) {
                Some(cache) => cache.get(key).await,
                None => {
                    error!("Failed to get cache for rule #{} from cache map", rule_id);
                    None
                }
            },
        }
    }

    pub fn resolve_task_upstream(&self, task_type: &Task) -> String {
        match &task_type {
            Task::PypiIndexTask { pkg_name, .. } => {
                format!("{}/{}", &self.config.builtin.pypi_index.upstream, pkg_name)
            }
            Task::PypiPackagesTask { pkg_path, .. } => format!(
                "{}/{}",
                &self.config.builtin.pypi_packages.upstream, pkg_path
            ),
            Task::AnacondaTask { path } => {
                format!("{}/{}", &self.config.builtin.anaconda.upstream, path)
            }
            Task::Others { url, .. } => url.clone(),
        }
    }

    pub fn add_cache(&mut self, rule_id: RuleId, cache: Arc<dyn CachePolicy>) {
        // insert cache into cache map
        self.cache_map.insert(rule_id, cache);
    }

    pub fn get_cache_for_cache_rule(&self, rule_id: RuleId) -> Option<Arc<dyn CachePolicy>> {
        match self.cache_map.get(&rule_id) {
            Some(cache) => Some(cache.clone()),
            None => None,
        }
    }
}
