use crate::repository::*;
use crate::wait_flag::*;
use std::collections::{BTreeSet, HashMap};

pub struct HbState {
    pub repos: HashMap<String, RepositoryState>,
    pub transient_dependencies: HashMap<String, BTreeSet<String>>,
    pub transient_dependents: HashMap<String, BTreeSet<String>>,
    //dependents: HashMap<String, BTreeSet<String>>,
    pub repos_in_build: WaitFlag<usize>,
}

impl HbState {
    pub fn new(repos: HashMap<String, RepositoryState>) -> Self {
        println!("REPOS: {repos:?}");

        let transient_dependencies = build_transient_depmap(&repos);

        println!("DEPMAP: {transient_dependencies:?}");

        let transient_dependents = inverse_depmap(&transient_dependencies);
        println!("INV_DEPMAP: {transient_dependents:?}");

        //let dependents = build_dependents(&repos);
        //println!("dependents: {dependents:?}");

        Self {
            repos,
            transient_dependencies,
            transient_dependents,
            //dependents,
            repos_in_build: Default::default(),
        }
    }
}

/// Builds a list of transient dependencies each repository contains
fn build_transient_depmap(
    repos: &HashMap<String, RepositoryState>,
) -> HashMap<String, BTreeSet<String>> {
    let mut out = HashMap::default();

    for (name, r) in repos {
        let mut out_set = BTreeSet::default();

        let mut deps = r.repo.dependencies.clone();

        while let Some((r, d)) = deps.pop().and_then(|d| repos.get(&d).zip(Some(d))) {
            out_set.insert(d);
            deps.extend(r.repo.dependencies.clone());
        }

        out.insert(name.clone(), out_set);
    }

    out
}

fn inverse_depmap(depmap: &HashMap<String, BTreeSet<String>>) -> HashMap<String, BTreeSet<String>> {
    let mut out: HashMap<String, BTreeSet<String>> = depmap
        .keys()
        .cloned()
        .map(|k| (k, Default::default()))
        .collect();

    for (k, deps) in depmap {
        for dep in deps {
            out.get_mut(dep).unwrap().insert(k.clone());
        }
    }

    out
}

/*fn build_dependents(repos: &HashMap<String, RepositoryState>) -> HashMap<String, BTreeSet<String>> {
    let mut ret: HashMap<String, BTreeSet<String>> = repos
        .keys()
        .cloned()
        .map(|k| (k, Default::default()))
        .collect();

    for (k, r) in repos {
        for d in &r.repo.dependencies {
            ret.get_mut(d).unwrap().insert(k.clone());
        }
    }

    ret
}*/
