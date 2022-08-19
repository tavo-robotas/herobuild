use crate::hb_state::*;
use crate::wait_flag::*;
use anyhow::{anyhow, Result};
use async_std::sync::{Mutex, RwLock};
use bollard::{
    container::{
        self, CreateContainerOptions, KillContainerOptions, StartContainerOptions,
        WaitContainerOptions,
    },
    models::{HostConfig, MountPoint},
    Docker,
};
use futures::future::join_all;
use log::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio_stream::StreamExt;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshKey {
    publickey: Option<PathBuf>,
    privatekey: Option<PathBuf>,
    passphrase: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct GitDeployment {
    url: String,
    branch: Option<String>,
    pre_checkout: Option<String>,
    post_checkout: Option<String>,
    key: Option<SshKey>,
}

impl GitDeployment {
    fn ssh_callbacks<'a>(
        prev_req: &'a mut Option<(String, Option<String>, git2::CredentialType)>,
    ) -> git2::RemoteCallbacks<'a> {
        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.credentials(move |url, username_from_url, allowed_types| {
            let username = username_from_url.unwrap_or("git");

            let cur_req = Some((
                url.to_string(),
                username_from_url.map(|v| v.to_string()),
                allowed_types,
            ));

            if prev_req == &cur_req
                && allowed_types
                    .intersects(git2::CredentialType::SSH_KEY | git2::CredentialType::SSH_CUSTOM)
            {
                info!("Backup ssh-agent credential strategy");
                // Load creds into ssh agent by performing a connection
                Command::new("ssh")
                    //.args(["-tt"])
                    .args(["-q", "-o", "BatchMode=yes"])
                    .arg(format!(
                        "{}",
                        url.split_once(':').map(|(a, _)| a).unwrap_or(url)
                    ))
                    .output()
                    .ok();
            }

            *prev_req = cur_req;

            let res = git2::Cred::ssh_key_from_agent(username).or_else(|_| {
                git2::Cred::credential_helper(
                    &git2::Config::open_default()?,
                    url,
                    username_from_url,
                )
            })?;

            Ok(res)
        });
        callbacks
    }

    fn update_submodules(repo: &git2::Repository) -> Result<()> {
        for mut sub in repo.submodules()? {
            let data = &mut None;

            let mut update_opts = git2::SubmoduleUpdateOptions::new();

            let mut fo = git2::FetchOptions::new();
            fo.remote_callbacks(Self::ssh_callbacks(data));

            update_opts.fetch(fo);

            sub.update(true, Some(&mut update_opts))?;
            let sub = sub.open()?;
            Self::update_submodules(&sub)?;
        }

        Ok(())
    }

    pub fn ensure_created(&self, src: &Path) -> Result<bool> {
        if fs::read_dir(&src).is_err() {
            let data = &mut None;

            let mut builder = git2::build::RepoBuilder::new();

            let mut fo = git2::FetchOptions::new();
            fo.remote_callbacks(Self::ssh_callbacks(data));

            builder.fetch_options(fo);

            if let Some(branch) = self.branch.as_deref() {
                builder.branch(branch);
            }

            let repo = builder.clone(&self.url, &src)?;
            Self::update_submodules(&repo)?;

            if let Some(post_checkout) = self.post_checkout.as_deref() {
                let output = Command::new("/bin/sh")
                    .args(["-c", post_checkout])
                    .current_dir(&src)
                    .output()?;

                if !output.status.success() {
                    return Err(anyhow!(
                        "Failed to run post_checkout: {}",
                        String::from_utf8_lossy(&output.stdout)
                    ));
                }
            }

            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn update_src(&self, src: &Path) -> Result<bool> {
        // Update the source repository

        info!("OPENING REPO");

        let repo = git2::Repository::open(&src)?;

        info!("OPENED REPO");

        let main_ref = self.branch.as_deref().unwrap_or("main");

        let mut remote = repo.find_remote("origin")?;

        info!("OPENED REMOTE");

        // fetch a commit
        let data = &mut None;
        let mut fo = git2::FetchOptions::new();
        fo.remote_callbacks(Self::ssh_callbacks(data));
        fo.download_tags(git2::AutotagOption::All);
        let res = remote.fetch(&[main_ref], Some(&mut fo), None);

        println!("RES {res:?}");

        res?;

        info!("FETCHED");

        // checkout the commit
        let head = repo.find_reference("HEAD")?;
        let head_commit = repo.reference_to_annotated_commit(&head)?;
        let fetch_head = repo.find_reference("FETCH_HEAD")?;
        let commit = repo.reference_to_annotated_commit(&fetch_head)?;

        info!("{} {}", head_commit.id(), commit.id());

        if head_commit.id() == commit.id() {
            return Ok(false);
        }

        let refname = format!("refs/heads/{main_ref}");

        match repo.find_reference(&refname) {
            Ok(mut r) => {
                let name = match r.name() {
                    Some(s) => s.to_string(),
                    None => String::from_utf8_lossy(r.name_bytes()).to_string(),
                };

                let msg = format!("Fast-Forward: Setting {} to id: {}", name, commit.id());
                println!("{msg}");

                r.set_target(commit.id(), &msg)?;
                repo.set_head(&name)?;
            }
            Err(_) => {
                let msg = format!("Setting {main_ref} to {}", commit.id());
                println!("{msg}");

                repo.reference(&refname, commit.id(), true, &msg)?;

                repo.set_head(&refname)?;
            }
        }

        if let Some(pre_checkout) = self.pre_checkout.as_deref() {
            let output = Command::new("/bin/sh")
                .args(["-c", pre_checkout])
                .current_dir(&src)
                .output()?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to run pre_checkout: {}",
                    String::from_utf8_lossy(&output.stdout)
                ));
            }
        }

        repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))?;
        Self::update_submodules(&repo)?;

        if let Some(post_checkout) = self.post_checkout.as_deref() {
            let output = Command::new("/bin/sh")
                .args(["-c", post_checkout])
                .current_dir(&src)
                .output()?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to run post_checkout: {}",
                    String::from_utf8_lossy(&output.stdout)
                ));
            }
        }

        Ok(true)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repository {
    /// Name of the repository
    pub name: String,
    /// Path to the source directory
    src: String,
    /// Path to the output repository
    repo: String,
    /// Extra arguments to pass to catkin-bloom
    extra_args: Option<Vec<String>>,
    /// Extra environment variables to pass
    env: Option<Vec<String>>,
    /// Repositories this repo depends on
    pub dependencies: Vec<String>,
    /// Information needed to deploy the source repository
    git_deployment: Option<GitDeployment>,
}

impl Repository {
    pub fn check_for_src_changes(&self, path: &Path) -> Result<bool> {
        if let Some(git_deployment) = &self.git_deployment {
            let src = path.join(&self.src);
            git_deployment.update_src(&src)
        } else {
            Ok(false)
        }
    }

    pub fn initialize(mut self, path: &Path, tags: &[&str]) -> Result<(Self, bool)> {
        let src = path.join(self.src);

        let repo_changed = if let Some(git_deployment) = &self.git_deployment {
            info!("{git_deployment:?}");
            if !git_deployment.ensure_created(&src)? {
                info!("Updating src");
                git_deployment.update_src(&src)?
            } else {
                true
            }
        } else {
            false
        };
        //let repo_changed = true;

        info!("{repo_changed}");

        let src = src.canonicalize()?;
        self.src = src
            .to_str()
            .ok_or_else(|| anyhow!("Invalid src path"))?
            .into();

        let repo = path.join(self.repo);

        fs::create_dir_all(&repo)?;

        let repo = repo.canonicalize()?;
        self.repo = repo
            .to_str()
            .ok_or_else(|| anyhow!("Invalid repo path"))?
            .into();

        for tag in tags {
            fs::create_dir_all(repo.join(tag))?;
        }

        Ok((self, repo_changed))
    }

    fn dependencies<'a>(
        &'a self,
        transient_deps: &'a HashMap<String, BTreeSet<String>>,
    ) -> impl Iterator<Item = &'a str> + 'a {
        transient_deps
            .get(&self.name)
            .into_iter()
            .flat_map(|deps| deps.iter().map(|s| s.as_str()))
    }

    /// Returns a destination -> source map.
    pub fn mounts(
        &self,
        tag: &str,
        repos: &HashMap<String, impl AsRef<Repository>>,
        transient_deps: &HashMap<String, BTreeSet<String>>,
    ) -> HashMap<String, String> {
        let mut ret = HashMap::default();

        ret.insert("/src".to_string(), self.src.clone());
        ret.insert("/repo".to_string(), format!("{}/{tag}", self.repo));

        for dep in self.dependencies(transient_deps) {
            if let Some(r) = repos.get(dep).map(AsRef::as_ref) {
                ret.insert(format!("/deps/{}", r.name), format!("{}/{tag}", r.repo));
            }
        }

        ret
    }

    /// Returns the exec command for the container
    pub fn cmd(&self, transient_deps: &HashMap<String, BTreeSet<String>>) -> Vec<String> {
        let mut ret = vec!["-j8".to_string()];

        let deps = self.dependencies(transient_deps);

        let deps = deps
            .into_iter()
            .map(|d| format!("/deps/{}", d))
            .collect::<Vec<_>>();

        let extra_repos = deps.join(",");

        if !extra_repos.is_empty() {
            ret.extend(["-e".to_string(), extra_repos]);
        }

        ret.extend(["-r".to_string(), "/repo".to_string(), "/src".to_string()]);

        ret.extend(self.extra_args.clone().into_iter().flatten());

        ret
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepositoryCache {
    repo_hash: String,
    /// Dirty flag indicating packages need to be rebuilt due to source or dependency changes
    #[serde(default)]
    pub dirty: bool,
    /// Image tag, container hash map
    docker_containers: HashMap<String, String>,
}

impl RepositoryCache {
    pub fn sync_hash(&mut self, repo: &Repository) -> bool {
        let bytes = toml::to_vec(repo).unwrap_or_default();
        let hash = sha256::digest_bytes(&bytes);
        let eq = self.repo_hash == hash;
        self.repo_hash = hash;
        !eq
    }

    pub async fn sync(
        &mut self,
        docker: &Docker,
        repo: &Repository,
        repos: &HashMap<String, impl AsRef<Repository>>,
        transient_deps: &HashMap<String, BTreeSet<String>>,
        tags: &[(&str, &str, String, String)],
    ) -> Result<bool> {
        let mut tags = tags
            .iter()
            .cloned()
            .map(|(p, t, combo, id)| (id, (p, t, combo)))
            .collect::<BTreeMap<_, _>>();

        println!("{tags:?}");

        let mut to_remove = vec![];
        let cmd = repo.cmd(transient_deps);

        fn mounts_match(
            mounts: &HashMap<String, String>,
            docker_mounts: Option<&Vec<MountPoint>>,
        ) -> bool {
            let mut cnt = 0;

            if let Some(docker_mounts) = docker_mounts {
                for mp in docker_mounts {
                    let srcdest = mp.destination.as_ref().zip(mp.source.as_ref());

                    match srcdest {
                        Some((dest, src)) if mounts.get(dest) == Some(src) => {
                            cnt += 1;
                        }
                        _ => {}
                    }
                }
            }

            cnt == mounts.len()
        }

        fn env_match(target_env: Option<&Vec<String>>, docker_env: Option<&Vec<String>>) -> bool {
            if let Some(env) = target_env {
                let mut to_remove = env
                    .iter()
                    .map(core::ops::Deref::deref)
                    .collect::<BTreeSet<&str>>();

                for v in docker_env.into_iter().flatten() {
                    to_remove.remove(v.as_str());
                }

                to_remove.is_empty()
            } else {
                true
            }
        }

        for (k, v) in &mut self.docker_containers {
            let cont = docker.inspect_container(v, None).await;

            if let Ok(cont) = cont {
                let image = cont.image.clone().unwrap_or_default();

                let (platform_match, mounts_match) = if let Some((_, tag, combo)) = tags.get(&image)
                {
                    (
                        k.contains(combo),
                        mounts_match(
                            &repo.mounts(tag, repos, transient_deps),
                            cont.mounts.as_ref(),
                        ),
                    )
                } else {
                    (false, false)
                };

                match cont {
                    cont if !image.is_empty()
                        && platform_match
                        && mounts_match
                        && cont.config.as_ref().and_then(|c| c.cmd.as_ref()) == Some(&cmd)
                        && env_match(
                            repo.env.as_ref(),
                            cont.config.as_ref().and_then(|c| c.env.as_ref()),
                        ) =>
                    {
                        tags.remove(&image);
                    }
                    cont => {
                        println!(
                            "{:?} {:?} {} {} {} {}",
                            image,
                            tags.get(&image),
                            platform_match,
                            mounts_match,
                            cont.config.as_ref().and_then(|c| c.cmd.as_ref()) == Some(&cmd),
                            env_match(
                                repo.env.as_ref(),
                                cont.config.as_ref().and_then(|c| c.env.as_ref()),
                            )
                        );
                        to_remove.push((k.clone(), v.clone()));
                    }
                }
            } else {
                println!("ALT ROUTE {}", repo.name);
            }
        }

        let mut changed = false;

        for (k, v) in to_remove {
            self.docker_containers.remove(&k);
            changed = true;

            if docker.inspect_container(&v, None).await.is_ok() {
                docker
                    .kill_container(&v, Some(KillContainerOptions { signal: "SIGKILL" }))
                    .await
                    .ok();
                docker.remove_container(&v, None).await?;
            }
        }

        for (id, (_, tag, combo)) in tags {
            let mounts = repo.mounts(&tag, repos, transient_deps);

            changed = true;

            let options = Some(CreateContainerOptions {
                name: format!("herobuild-{}-{combo}", repo.name),
            });

            let host_config = Some(HostConfig {
                binds: Some(mounts.iter().map(|(d, s)| format!("{s}:{d}")).collect()),
                ..Default::default()
            });

            let config = container::Config {
                image: Some(id),
                host_config,
                cmd: Some(cmd.clone()),
                env: repo.env.clone(),
                ..Default::default()
            };

            println!("HI");

            let cont = docker.create_container(options, config).await?;

            println!("HI2");

            self.docker_containers.insert(combo, cont.id);
        }

        Ok(changed)
    }

    pub async fn clear_containers(&mut self, docker: &Docker) -> Result<()> {
        for (_, v) in &mut self.docker_containers {
            if docker.inspect_container(v, None).await.is_ok() {
                docker
                    .kill_container(v, Some(KillContainerOptions { signal: "SIGKILL" }))
                    .await
                    .ok();
                docker.remove_container(v, None).await?;
            }
        }

        self.docker_containers.clear();

        Ok(())
    }

    async fn build(&self, docker: &Docker, repo: &Repository) -> Result<()> {
        let mut futures = vec![];

        for (plat, cont) in &self.docker_containers {
            futures.push(async move {
                println!("Building {} {plat}: {cont}", repo.name);

                docker
                    .start_container(cont, None::<StartContainerOptions<String>>)
                    .await?;

                let options = Some(WaitContainerOptions {
                    condition: "not-running",
                });

                let mut stream = docker.wait_container(cont, options);

                while stream.next().await.is_some() {}

                anyhow::Ok(())
            })
        }

        for r in join_all(futures).await {
            r?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct RepositoryState {
    pub repo: Repository,
    pub cache_path: PathBuf,
    pub cache: RwLock<RepositoryCache>,
    pub prio_lock: Mutex<()>,
    pub needs_build: WaitFlag<bool>,
}

impl RepositoryState {
    pub fn with_path(mut path: PathBuf, tags: &[&str]) -> Result<Self> {
        info!("{path:?}");
        let b = fs::read(&path)?;
        info!("{b:?}");
        let repo: Repository = toml::from_slice(&b)?;
        info!("{repo:?}");

        let mut cache_path = path.clone();
        cache_path.set_extension("toml.cache");

        let mut cache: RepositoryCache = fs::read(&cache_path)
            .and_then(|b| toml::from_slice(&b).map_err(Into::into))
            .unwrap_or_default();

        path.pop();

        let (repo, dirty) = repo.initialize(&path, tags)?;

        cache.dirty = cache.dirty || dirty;

        let needs_build = WaitFlag::new(cache.dirty);

        let cache = RwLock::new(cache);

        Ok(Self {
            repo,
            cache_path,
            cache,
            prio_lock: Default::default(),
            needs_build,
        })
    }

    pub async fn write_cache(&self) -> Result<()> {
        let mut cache = fs::File::create(&self.cache_path)?;
        cache.write_all(&toml::to_vec(&*self.cache.read().await)?)?;
        Ok(())
    }

    pub async fn build(&self, docker: &Docker, state: &HbState) -> Result<()> {
        info!("- Building {}", self.repo.name);

        info!("+ Entered exclusive mode for {}", self.repo.name);

        let deps = state.transient_dependencies.get(&self.repo.name).unwrap();
        let trans_dependents = state.transient_dependents.get(&self.repo.name).unwrap();

        info!("Blocking dependents of {}", self.repo.name);

        // Priority lock self
        info!("- Prio lock {}", self.repo.name);
        let priority = self.prio_lock.lock().await;
        info!("+ Prio lock {}", self.repo.name);

        // Build only when all dependencies and dependents have finished building
        info!("Dependency lock {}", self.repo.name);

        let mut tmp_deps = deps
            .iter()
            .map(|d| (d.clone(), state.repos.get(d).unwrap()))
            .collect::<Vec<_>>();

        // This is a more peculiar locking strategy.
        //
        // Basically we want to give priority to dependencies, but we also need to lock
        // everything. If we were to simply lock everything in order we would block the
        // dependency that may want to build, and screw up the order of operations.
        // Instead, what we do is try read-locking each dependency. If any attempt fails,
        // then we unlock everything, read lock the blocking lock, and try locking
        // everything again. We prevent starvation through simple fact of dependency
        // prioritization.
        let dependency_locks = {
            let mut locks = vec![];

            let unlock_all = |locks: &mut Vec<_>, tmp_deps: &mut Vec<_>| {
                for (_, d) in locks.drain(0..) {
                    tmp_deps.push(d);
                }
            };

            while let Some(data) = tmp_deps.pop() {
                let (d, r) = &data;

                // Wait until it is ready for blocking
                info!("- wait built {d} on {}", self.repo.name);

                // Wait for the build to be complete
                if r.needs_build.get().await {
                    // If need to be blocking, unlock everything first
                    unlock_all(&mut locks, &mut tmp_deps);
                    // Block wait at this step
                    r.needs_build.wait(false).await;
                }

                info!("+ wait built {d} on {}", self.repo.name);

                // Grab the priority lock
                if r.prio_lock.try_lock().is_none() {
                    unlock_all(&mut locks, &mut tmp_deps);
                    r.prio_lock.lock().await;
                }
                info!("+ grab prio of {d} on {}", self.repo.name);

                let cache = if let Some(cache) = r.cache.try_read() {
                    // Successfully locked without blocking!
                    cache
                } else {
                    // Unlock everything, reset the loop
                    unlock_all(&mut locks, &mut tmp_deps);
                    // Block on this one so as to save CPU cycles - we are only blocking
                    // this dependency after all!!!
                    r.cache.read().await
                };

                locks.push((cache, data));
            }

            locks
        };

        // Grab write lock right here
        info!("- Write lock {}", self.repo.name);
        let mut cache = self.cache.write().await;
        info!("+ Write locked {}", self.repo.name);

        // Drop the priority
        std::mem::drop(priority);

        // Lock all of the dependencies
        info!("- Dependent lock {}", self.repo.name);

        let dependent_locks = join_all(trans_dependents.iter().map(|d| {
            info!("{} lock {d} (dependent)", self.repo.name);
            async move { (d, state.repos.get(d).unwrap().cache.read().await) }
        }))
        .await;

        info!("+ Dependent locked {}", self.repo.name);

        // Signal all dependents to build before building

        self.needs_build.set(false).await;

        for d in trans_dependents {
            //println!("{d} indirectly dirtied {}", repo.repo.name);
            let dep_repo = state.repos.get(d).unwrap();
            dep_repo.needs_build.set(true).await;
        }

        // Update the repo
        tokio::task::block_in_place(|| {
            self.repo.check_for_src_changes(Path::new(&self.repo.repo))
        })?;

        // Build self
        cache.build(docker, &self.repo).await?;
        info!("BUILT {}", self.repo.name);

        cache.dirty = false;

        std::mem::drop(cache);
        std::mem::drop(dependent_locks);
        std::mem::drop(dependency_locks);

        Ok(())
    }
}

impl AsRef<Repository> for RepositoryState {
    fn as_ref(&self) -> &Repository {
        &self.repo
    }
}

impl Drop for RepositoryState {
    fn drop(&mut self) {
        smol::block_on(async {
            self.cache.write().await.sync_hash(&self.repo);
            self.write_cache().await.ok();
        });
    }
}
