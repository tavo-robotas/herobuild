use anyhow::Result;
use bollard::Docker;
use futures::future::join_all;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::sync::Arc;

mod hb_state;
mod repository;
mod wait_flag;

use hb_state::*;
use repository::*;
use wait_flag::*;

use bollard::image::{CreateImageOptions, TagImageOptions};
use log::*;
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let defs = "herobuild";

    let tags = &["melodic-bionic"];
    let platforms = &["linux/arm64", "linux/amd64"];

    println!("READ ALL");

    let repos = fs::read_dir(defs)?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension() == Some(OsStr::new("toml")))
        .map(|e| RepositoryState::with_path(e.path(), tags))
        .inspect(|r| println!("{r:?}"))
        .filter_map(Result::ok)
        .map(|r| (r.repo.name.clone(), r))
        .collect::<HashMap<_, _>>();

    let state = Arc::new(HbState::new(repos));

    println!("Hello, world!");

    println!("{:?}", state.repos);

    let docker = Arc::new(Docker::connect_with_local_defaults()?);

    let mut ids = vec![];

    for plat in platforms {
        for tag in tags {
            let (combo, id) = pull_image(&docker, plat, tag).await?;
            ids.push((*plat, *tag, combo, id));
        }
    }

    join_all(state.repos.values().map(|r| async {
        r.cache
            .write()
            .await
            .sync(
                &docker,
                &r.repo,
                &state.repos,
                &state.transient_dependencies,
                &ids,
            )
            .await
            .unwrap();
    }))
    .await;

    let keep_building = Arc::new(WaitFlag::new(true));

    let futures = state
        .repos
        .keys()
        .map(|name| {
            println!("Root build {name}");
            let docker = docker.clone();
            let state = state.clone();
            let name = name.to_string();
            let keep_building = keep_building.clone();
            tokio::spawn(async move {
                let repo = state.repos.get(&name).unwrap();

                let mut buildc = 0;

                while tokio::select!(
                    val = async {
                        keep_building.wait(false).await;
                        state.repos_in_build.wait(0).await;
                        false
                    } => val,
                    val = async {
                        repo.needs_build.wait(true).await;
                        keep_building.get().await || state.repos_in_build.get().await != 0
                    } => val,
                ) {
                    state.repos_in_build.modify(|v| *v += 1).await;
                    repo.build(&*docker, &state).await?;
                    buildc += 1;
                    state.repos_in_build.modify(|v| *v -= 1).await;

                    // Prevent starvation by letting other tasks take over
                    tokio::task::yield_now().await;

                    if keep_building.get().await {
                        //repo.needs_build.set(true).await;
                    }
                }

                println!("FINISH {name}");

                Result::<_, anyhow::Error>::Ok((name, buildc))
            })
        })
        .collect::<Vec<_>>();

    let monitor_on = Arc::new(WaitFlag::new(true));

    let monitor = {
        let state = state.clone();
        let keep_building = monitor_on.clone();

        tokio::spawn(async move {
            while keep_building.get().await {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                for (n, r) in &state.repos {
                    let needs_build = r.needs_build.get().await;
                    let prio_locked = r.prio_lock.try_lock().is_none();
                    let cache_readable = r.cache.try_read().is_some();
                    println!("{n:>10}: {needs_build} {prio_locked} {cache_readable}",);
                }
            }
        })
    };

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    keep_building.set(false).await;

    /*while building {
        //println!("Iter {iter}");

        iter += 1;
        building = false;

        if !futures.is_empty() {
            building = true;
        }
    }*/

    let mut total_builds = 0;

    monitor_on.set(false).await;

    monitor.await?;

    // Fail if any loop failed
    let mut stats: Vec<(String, usize)> = join_all(futures)
        .await
        .into_iter()
        .collect::<core::result::Result<Result<_>, _>>()??;

    stats.sort_by_key(|(_, v)| *v);

    for (repo, builds) in stats {
        println!("{repo}: {builds}");
        total_builds += builds;
    }

    println!("Builds processed: {total_builds}",);

    Ok(())
}

async fn pull_image(docker: &Docker, plat: &str, tag: &str) -> Result<(String, String)> {
    let combo = plat.replace("/", "-") + "-" + tag;
    let dl_repo = "docker.io/tavorobotas/catkin-bloom";
    let repo = "hbcache/catkin-bloom";

    let image_tag = format!("{dl_repo}:{tag}");
    let cached_image = format!("{repo}:{combo}");

    loop {
        // Check if cached image exists
        if let Ok(image) = docker.inspect_image(&cached_image).await {
            // Check if architecture matches
            if image
                .os
                .zip(image.architecture)
                .map(|(a, b)| format!("{a}/{b}"))
                .as_deref()
                == Some(plat)
            {
                if let Some(id) = image.id {
                    return Ok((combo, id));
                }
            }
        }

        // If not, pull a new image

        info!("Pulling {tag} plat {plat} as {combo}");

        let ret = docker
            .create_image(
                Some(CreateImageOptions {
                    from_image: image_tag.as_str(),
                    platform: plat,
                    ..Default::default()
                }),
                None,
                None,
            )
            .collect::<Vec<_>>()
            .await;

        println!("{ret:?}");

        // Tag the image if correct platform combo is provided
        if let Ok(image) = docker.inspect_image(&image_tag).await {
            let this_plt = image
                .os
                .zip(image.architecture)
                .map(|(a, b)| format!("{a}/{b}"));

            if this_plt.as_deref() == Some(plat) {
                if let Some(id) = image.id {
                    println!("TAGGING {id} as {repo}:{combo}");
                    docker
                        .tag_image(&id, Some(TagImageOptions { repo, tag: &combo }))
                        .await?;
                }
            }
        }
    }
}
