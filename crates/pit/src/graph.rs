//! The task graph: validation, planning groups per OS, selecting tasks,
//! affected-task computation and action keys.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;

use crate::model::{PitConfig, TaskDef};

/// Checks that deps exist within their group and that there are no cycles.
pub fn validate(config: &PitConfig) -> Result<()> {
    for (gname, group) in &config.groups {
        for (tname, task) in &group.tasks {
            for dep in &task.deps {
                if !group.tasks.contains_key(dep) {
                    bail!(
                        "task {gname}.{tname} depends on {dep:?}, which is not a task in group {gname} \
                         (deps must be in the same group)"
                    );
                }
            }
            // A sharded task is spread over several groups, so nothing in
            // its own group can wait for all of its shards.
            if task.shards.unwrap_or(1) > 1 {
                if let Some((other, _)) = group.tasks.iter().find(|(_, t)| t.deps.contains(tname)) {
                    bail!(
                        "task {gname}.{other} depends on {tname}, which is sharded; \
                         tasks cannot depend on a sharded task"
                    );
                }
            }
            if task.shards == Some(0) {
                bail!("task {gname}.{tname}: shards must be at least 1");
            }
            for pattern in task.inputs.iter().flatten().chain(&task.outputs) {
                Glob::new(pattern.strip_prefix('!').unwrap_or(pattern)).map_err(|e| {
                    anyhow::anyhow!("task {gname}.{tname}: bad glob {pattern:?}: {e}")
                })?;
            }
        }
        topo_order(&group.tasks).map_err(|e| anyhow::anyhow!("group {gname}: {e}"))?;
    }
    Ok(())
}

/// Tasks in dependency order. Errors on a cycle.
pub fn topo_order(tasks: &BTreeMap<String, TaskDef>) -> Result<Vec<String>> {
    let mut order = Vec::new();
    let mut state: BTreeMap<&str, u8> = BTreeMap::new(); // 1 = visiting, 2 = done
    fn visit<'a>(
        name: &'a str,
        tasks: &'a BTreeMap<String, TaskDef>,
        state: &mut BTreeMap<&'a str, u8>,
        order: &mut Vec<String>,
        stack: &mut Vec<&'a str>,
    ) -> Result<()> {
        match state.get(name) {
            Some(2) => return Ok(()),
            Some(1) => {
                stack.push(name);
                bail!("dependency cycle: {}", stack.join(" -> "));
            }
            _ => {}
        }
        state.insert(name, 1);
        stack.push(name);
        if let Some(task) = tasks.get(name) {
            for dep in &task.deps {
                visit(dep, tasks, state, order, stack)?;
            }
        }
        stack.pop();
        state.insert(name, 2);
        order.push(name.to_string());
        Ok(())
    }
    for name in tasks.keys() {
        visit(name, tasks, &mut state, &mut order, &mut Vec::new())?;
    }
    Ok(order)
}

/// A group instance to schedule: one per group per OS.
#[derive(Debug, Clone, Serialize)]
pub struct PlannedGroup {
    pub name: String,
    pub os: String,
    /// Task name and definition, in dependency order.
    pub tasks: Vec<(String, TaskDef)>,
}

/// Expands the config into schedulable groups. `selected` filters tasks by
/// name (`task` or `group.task`); their deps are always included.
pub fn plan(config: &PitConfig, selected: &[String]) -> Result<Vec<PlannedGroup>> {
    let mut groups = Vec::new();
    let mut matched: BTreeSet<&str> = BTreeSet::new();
    for (gname, group) in &config.groups {
        let mut keep: BTreeSet<String> = BTreeSet::new();
        if selected.is_empty() {
            keep.extend(group.tasks.keys().cloned());
        } else {
            for sel in selected {
                let (g, t) = match sel.split_once('.') {
                    Some((g, t)) => (Some(g), t),
                    None => (None, sel.as_str()),
                };
                if g.is_some_and(|g| g != gname) || !group.tasks.contains_key(t) {
                    continue;
                }
                matched.insert(sel);
                add_with_deps(t, &group.tasks, &mut keep);
            }
        }
        if keep.is_empty() {
            continue;
        }
        let order = topo_order(&group.tasks)?;
        let tasks: Vec<(String, TaskDef)> = order
            .into_iter()
            .filter(|t| keep.contains(t))
            .map(|t| (t.clone(), group.tasks[&t].clone()))
            .collect();
        for os in &group.os {
            let name = if group.os.len() > 1 {
                format!("{gname}@{os}")
            } else {
                gname.clone()
            };
            groups.extend(shard_groups(&name, os, &tasks));
        }
    }
    if let Some(missing) = selected.iter().find(|s| !matched.contains(s.as_str())) {
        bail!("no task named {missing:?} in the pit config");
    }
    Ok(groups)
}

/// Expands a group with sharded tasks into one group per shard (`ci#1`,
/// `ci#2`, ...). Each shard group gets the shard of every sharded task plus
/// their deps (setup is repeated per shard, and shared through the cache);
/// the group's other tasks go into the first shard.
fn shard_groups(name: &str, os: &str, tasks: &[(String, TaskDef)]) -> Vec<PlannedGroup> {
    let count = tasks
        .iter()
        .map(|(_, t)| t.shards.unwrap_or(1))
        .max()
        .unwrap_or(1);
    if count <= 1 {
        return vec![PlannedGroup {
            name: name.to_string(),
            os: os.to_string(),
            tasks: tasks.to_vec(),
        }];
    }
    let by_name: BTreeMap<String, TaskDef> = tasks.iter().cloned().collect();
    let sharded: Vec<&(String, TaskDef)> = tasks
        .iter()
        .filter(|(_, t)| t.shards.unwrap_or(1) > 1)
        .collect();
    let mut setup = BTreeSet::new();
    for (_, t) in &sharded {
        for d in &t.deps {
            add_with_deps(d, &by_name, &mut setup);
        }
    }
    (1..=count)
        .map(|k| {
            let tasks = tasks
                .iter()
                .filter_map(|(n, t)| {
                    let shards = t.shards.unwrap_or(1);
                    if shards > 1 {
                        (k <= shards).then(|| {
                            (
                                n.clone(),
                                TaskDef {
                                    shard: Some(k),
                                    ..t.clone()
                                },
                            )
                        })
                    } else if setup.contains(n) || k == 1 {
                        Some((n.clone(), t.clone()))
                    } else {
                        None
                    }
                })
                .collect();
            PlannedGroup {
                name: format!("{name}#{k}"),
                os: os.to_string(),
                tasks,
            }
        })
        .collect()
}

fn add_with_deps(name: &str, tasks: &BTreeMap<String, TaskDef>, keep: &mut BTreeSet<String>) {
    if !keep.insert(name.to_string()) {
        return;
    }
    for dep in &tasks[name].deps {
        add_with_deps(dep, tasks, keep);
    }
}

pub fn globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p)?);
    }
    Ok(b.build()?)
}

/// Input globs with gitignore-style exclusions: `["**", "!docs/**"]`.
pub struct InputMatcher {
    include: GlobSet,
    exclude: GlobSet,
}

impl InputMatcher {
    pub fn new(patterns: &[String]) -> Result<InputMatcher> {
        let (neg, pos): (Vec<String>, Vec<String>) =
            patterns.iter().cloned().partition(|p| p.starts_with('!'));
        let neg: Vec<String> = neg.into_iter().map(|p| p[1..].to_string()).collect();
        Ok(InputMatcher {
            include: globset(&pos)?,
            exclude: globset(&neg)?,
        })
    }

    pub fn for_task(task: &TaskDef) -> Result<InputMatcher> {
        InputMatcher::new(&input_globs(task))
    }

    pub fn is_match(&self, path: &str) -> bool {
        self.include.is_match(path) && !self.exclude.is_match(path)
    }
}

/// Inputs default to every tracked file, which is always correct, just less
/// cacheable.
pub fn input_globs(task: &TaskDef) -> Vec<String> {
    task.inputs
        .clone()
        .unwrap_or_else(|| vec!["**".to_string()])
}

/// Tasks (`group.task`) whose results can change because of `changed` files:
/// tasks with a matching input, plus everything that depends on them.
pub fn affected(config: &PitConfig, changed: &[String]) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for (gname, group) in &config.groups {
        let mut hit: BTreeSet<String> = BTreeSet::new();
        for (tname, task) in &group.tasks {
            let set = InputMatcher::for_task(task)?;
            if changed.iter().any(|f| set.is_match(f)) {
                hit.insert(tname.clone());
            }
        }
        // Propagate to dependents until nothing changes.
        loop {
            let before = hit.len();
            for (tname, task) in &group.tasks {
                if task.deps.iter().any(|d| hit.contains(d)) {
                    hit.insert(tname.clone());
                }
            }
            if hit.len() == before {
                break;
            }
        }
        out.extend(hit.into_iter().map(|t| format!("{gname}.{t}")));
    }
    // A config change can change any task.
    if changed
        .iter()
        .any(|f| crate::config::CONFIG_FILES.contains(&f.as_str()))
    {
        for (gname, group) in &config.groups {
            out.extend(group.tasks.keys().map(|t| format!("{gname}.{t}")));
        }
    }
    Ok(out)
}

/// Everything about a task except its input files and deps.
#[derive(Serialize)]
struct ConfigMaterial<'a> {
    run: &'a str,
    os: &'a str,
    arch: &'a str,
    inputs: &'a [String],
    outputs: &'a [String],
    backend: Option<&'a str>,
    image: Option<&'a str>,
    network: Option<bool>,
    timeout: Option<u64>,
    shard: Option<(u32, u32)>,
    env: Vec<(&'a str, String)>,
    /// Names only: secret values never enter the key.
    secrets: &'a [String],
}

/// What an action key is made of, kept so a cache miss can say what changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct KeyInputs {
    pub v: u32,
    /// Digest of the task definition, OS, arch and env values.
    pub config: String,
    /// Action keys of the task's deps, sorted.
    pub deps: Vec<String>,
    /// Matching input files as (path, git blob id or overlay digest), sorted.
    pub files: Vec<(String, String)>,
    /// Output of the task's `runtime` commands on the worker that ran it.
    /// Left out when empty, so keys of tasks without `runtime` do not change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime: Vec<(String, String)>,
}

impl KeyInputs {
    /// The action key: a hash of everything that can change a task's result.
    pub fn key(&self) -> String {
        crate::cas::digest(&serde_json::to_vec(self).unwrap_or_default())
    }
}

/// Like `key_inputs`, with the outputs of the task's `runtime` commands.
pub fn key_inputs_with_runtime(
    task: &TaskDef,
    os: &str,
    files: &[(String, String)],
    dep_keys: &[String],
    runtime: &std::collections::HashMap<String, String>,
) -> Result<KeyInputs> {
    let mut inputs = key_inputs(task, os, files, dep_keys)?;
    inputs.runtime = task
        .runtime
        .iter()
        .map(|cmd| {
            (
                cmd.clone(),
                runtime
                    .get(cmd)
                    .cloned()
                    .unwrap_or_else(|| "<not measured>".into()),
            )
        })
        .collect();
    Ok(inputs)
}

pub fn key_inputs(
    task: &TaskDef,
    os: &str,
    files: &[(String, String)],
    dep_keys: &[String],
) -> Result<KeyInputs> {
    let inputs = input_globs(task);
    let set = InputMatcher::new(&inputs)?;
    let mut matched: Vec<(String, String)> = files
        .iter()
        .filter(|(p, _)| set.is_match(p))
        .cloned()
        .collect();
    matched.sort();
    let mut env: Vec<(&str, String)> = task
        .env
        .iter()
        .map(|k| (k.as_str(), std::env::var(k).unwrap_or_default()))
        .collect();
    env.sort();
    let mut deps = dep_keys.to_vec();
    deps.sort();
    let config = ConfigMaterial {
        run: &task.run,
        os,
        arch: std::env::consts::ARCH,
        inputs: &inputs,
        outputs: &task.outputs,
        backend: task.backend.map(|b| b.as_str()),
        image: task.image.as_deref(),
        network: task.network,
        timeout: task.timeout,
        shard: task.shard.map(|k| (k, task.shards.unwrap_or(1))),
        env,
        secrets: &task.secrets,
    };
    Ok(KeyInputs {
        v: 3,
        config: crate::cas::digest(&serde_json::to_vec(&config)?),
        deps,
        files: matched,
        runtime: vec![],
    })
}

/// The action key: a hash of everything that can change a task's result.
pub fn action_key(
    task: &TaskDef,
    os: &str,
    files: &[(String, String)],
    dep_keys: &[String],
) -> Result<String> {
    Ok(key_inputs(task, os, files, dep_keys)?.key())
}

/// Why a task's key differs from its previous cached run, in a few words:
/// `"src/a.ts changed; build changed"`. `deps` names the current dep keys.
pub fn explain_miss(prev: &KeyInputs, cur: &KeyInputs, deps: &[(String, String)]) -> String {
    let mut reasons = Vec::new();
    let before_rt: BTreeMap<&str, &str> = prev
        .runtime
        .iter()
        .map(|(c, o)| (c.as_str(), o.as_str()))
        .collect();
    for (cmd, out) in &cur.runtime {
        match before_rt.get(cmd.as_str()) {
            Some(old) if old != out => reasons.push(format!("`{cmd}` changed ({old} → {out})")),
            Some(_) => {}
            None => reasons.push(format!("`{cmd}` is now part of the key ({out})")),
        }
    }
    if prev.config != cur.config {
        reasons.push("task definition or environment changed".to_string());
    }
    let before: std::collections::BTreeMap<&str, &str> = prev
        .files
        .iter()
        .map(|(p, i)| (p.as_str(), i.as_str()))
        .collect();
    let after: std::collections::BTreeMap<&str, &str> = cur
        .files
        .iter()
        .map(|(p, i)| (p.as_str(), i.as_str()))
        .collect();
    let mut changed: Vec<String> = Vec::new();
    for (path, id) in &after {
        match before.get(path) {
            None => changed.push(format!("{path} added")),
            Some(old) if old != id => changed.push(format!("{path} changed")),
            _ => {}
        }
    }
    for path in before.keys().filter(|p| !after.contains_key(*p)) {
        changed.push(format!("{path} removed"));
    }
    if !changed.is_empty() {
        let shown = changed.len().min(3);
        let mut text = changed[..shown].join(", ");
        if changed.len() > shown {
            text.push_str(&format!(" and {} more file(s)", changed.len() - shown));
        }
        reasons.push(text);
    }
    let prev_deps: BTreeSet<&str> = prev.deps.iter().map(String::as_str).collect();
    let changed_deps: Vec<&str> = deps
        .iter()
        .filter(|(_, k)| !prev_deps.contains(k.as_str()))
        .map(|(n, _)| n.as_str())
        .collect();
    if !changed_deps.is_empty() {
        reasons.push(format!("{} changed", changed_deps.join(", ")));
    }
    if reasons.is_empty() {
        "inputs changed".to_string()
    } else {
        reasons.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GroupDef;

    fn task(run: &str, deps: &[&str], inputs: Option<&[&str]>) -> TaskDef {
        TaskDef {
            run: run.into(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            inputs: inputs.map(|i| i.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    fn sample() -> PitConfig {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "install".into(),
            task("bun i", &[], Some(&["package.json", "bun.lock"])),
        );
        tasks.insert("lint".into(), task("lint", &["install"], Some(&["src/**"])));
        tasks.insert(
            "build".into(),
            task("build", &["install"], Some(&["src/**", "vite.config.ts"])),
        );
        tasks.insert("docs".into(), task("docs", &[], Some(&["docs/**"])));
        let mut groups = BTreeMap::new();
        groups.insert(
            "ci".into(),
            GroupDef {
                os: vec!["linux".into(), "windows".into()],
                tasks,
                ..Default::default()
            },
        );
        PitConfig {
            groups,
            ..Default::default()
        }
    }

    #[test]
    fn plans_one_group_per_os_in_dependency_order() {
        let groups = plan(&sample(), &[]).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "ci@linux");
        let names: Vec<_> = groups[0].tasks.iter().map(|(n, _)| n.as_str()).collect();
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("install") < pos("lint"));
        assert!(pos("install") < pos("build"));
    }

    #[test]
    fn selecting_a_task_pulls_in_its_deps_only() {
        let groups = plan(&sample(), &["lint".into()]).unwrap();
        let names: Vec<_> = groups[0].tasks.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, vec!["install", "lint"]);
        assert!(plan(&sample(), &["nope".into()]).is_err());
    }

    #[test]
    fn affected_follows_inputs_and_dependents() {
        let c = sample();
        let a = affected(&c, &["docs/intro.md".into()]).unwrap();
        assert_eq!(a.into_iter().collect::<Vec<_>>(), vec!["ci.docs"]);
        let a = affected(&c, &["bun.lock".into()]).unwrap();
        assert_eq!(a.len(), 3, "install change affects lint and build: {a:?}");
        assert!(affected(&c, &["README.md".into()]).unwrap().is_empty());
        assert_eq!(affected(&c, &["pit.config.ts".into()]).unwrap().len(), 4);
    }

    #[test]
    fn explains_why_a_task_missed_the_cache() {
        let t = task("build", &[], Some(&["src/**", "tsconfig.json"]));
        let files = |v: &[(&str, &str)]| {
            v.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect::<Vec<_>>()
        };
        let before = key_inputs(
            &t,
            "linux",
            &files(&[("src/a.ts", "1"), ("tsconfig.json", "1")]),
            &[],
        )
        .unwrap();
        let after = key_inputs(
            &t,
            "linux",
            &files(&[("src/a.ts", "1"), ("tsconfig.json", "2"), ("src/b.ts", "1")]),
            &[],
        )
        .unwrap();
        assert_eq!(
            explain_miss(&before, &after, &[]),
            "src/b.ts added, tsconfig.json changed"
        );
        let test = task("test", &["build"], Some(&["test/**"]));
        let a = key_inputs(&test, "linux", &[], &[before.key()]).unwrap();
        let b = key_inputs(&test, "linux", &[], &[after.key()]).unwrap();
        assert_eq!(
            explain_miss(&a, &b, &[("build".into(), after.key())]),
            "build changed"
        );
        let edited = task("build --prod", &[], Some(&["src/**", "tsconfig.json"]));
        let c = key_inputs(
            &edited,
            "linux",
            &files(&[("src/a.ts", "1"), ("tsconfig.json", "1")]),
            &[],
        )
        .unwrap();
        assert_eq!(
            explain_miss(&before, &c, &[]),
            "task definition or environment changed"
        );
        assert_eq!(
            before.key(),
            action_key(
                &t,
                "linux",
                &files(&[("src/a.ts", "1"), ("tsconfig.json", "1")]),
                &[]
            )
            .unwrap()
        );
    }

    #[test]
    fn sharded_tasks_split_into_groups_with_their_setup() {
        let mut tasks = BTreeMap::new();
        tasks.insert("install".into(), task("i", &[], None));
        tasks.insert("lint".into(), task("l", &["install"], None));
        let mut test = task("t", &["install"], None);
        test.shards = Some(3);
        tasks.insert("test".into(), test);
        let mut groups = BTreeMap::new();
        groups.insert(
            "ci".into(),
            GroupDef {
                os: vec!["linux".into()],
                tasks,
                ..Default::default()
            },
        );
        let config = PitConfig {
            groups,
            ..Default::default()
        };
        let planned = plan(&config, &[]).unwrap();
        let names: Vec<_> = planned.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["ci#1", "ci#2", "ci#3"]);
        let tasks_of = |i: usize| {
            planned[i]
                .tasks
                .iter()
                .map(|(n, t)| (n.as_str(), t.shard))
                .collect::<Vec<_>>()
        };
        // Setup in every shard, lint once, each shard its own index.
        assert_eq!(
            tasks_of(0),
            vec![("install", None), ("lint", None), ("test", Some(1))]
        );
        assert_eq!(tasks_of(2), vec![("install", None), ("test", Some(3))]);
        // Shards never share a cache key.
        let key =
            |i: usize| action_key(&planned[i].tasks.last().unwrap().1, "linux", &[], &[]).unwrap();
        assert_ne!(key(0), key(1));
        // Nothing may depend on a sharded task.
        let mut bad = config.clone();
        bad.groups
            .get_mut("ci")
            .unwrap()
            .tasks
            .get_mut("lint")
            .unwrap()
            .deps
            .push("test".into());
        assert!(validate(&bad).unwrap_err().to_string().contains("sharded"));
    }

    #[test]
    fn runtime_outputs_are_part_of_the_key() {
        let mut t = task("build", &[], Some(&["src/**"]));
        t.runtime = vec!["node --version".into()];
        let rt = |v: &str| {
            std::collections::HashMap::from([("node --version".to_string(), v.to_string())])
        };
        let a = key_inputs_with_runtime(&t, "linux", &[], &[], &rt("v22.11.0")).unwrap();
        let b = key_inputs_with_runtime(&t, "linux", &[], &[], &rt("v24.3.0")).unwrap();
        assert_ne!(a.key(), b.key());
        assert_eq!(
            explain_miss(&a, &b, &[]),
            "`node --version` changed (v22.11.0 → v24.3.0)"
        );
        // Tasks without `runtime` keep their keys.
        let plain = task("build", &[], Some(&["src/**"]));
        assert_eq!(
            key_inputs_with_runtime(&plain, "linux", &[], &[], &rt("x"))
                .unwrap()
                .key(),
            action_key(&plain, "linux", &[], &[]).unwrap()
        );
    }

    #[test]
    fn input_exclusions() {
        let m = InputMatcher::new(&["**".into(), "!**/*.md".into(), "!docs/**".into()]).unwrap();
        assert!(m.is_match("src/a.ts"));
        assert!(!m.is_match("README.md"));
        assert!(!m.is_match("docs/x.ts"));
    }

    #[test]
    fn rejects_cycles_and_cross_group_deps() {
        let mut c = sample();
        let g = c.groups.get_mut("ci").unwrap();
        g.tasks
            .get_mut("install")
            .unwrap()
            .deps
            .push("build".into());
        assert!(validate(&c).unwrap_err().to_string().contains("cycle"));
        let mut c = sample();
        c.groups
            .get_mut("ci")
            .unwrap()
            .tasks
            .get_mut("lint")
            .unwrap()
            .deps
            .push("other".into());
        assert!(validate(&c).is_err());
    }

    #[test]
    fn action_key_only_changes_with_matching_inputs() {
        let t = task("build", &[], Some(&["src/**"]));
        let files = vec![
            ("src/a.ts".to_string(), "1".to_string()),
            ("README.md".to_string(), "1".to_string()),
        ];
        let k1 = action_key(&t, "linux", &files, &[]).unwrap();
        let readme_changed = vec![
            ("src/a.ts".to_string(), "1".to_string()),
            ("README.md".to_string(), "2".to_string()),
        ];
        assert_eq!(k1, action_key(&t, "linux", &readme_changed, &[]).unwrap());
        let src_changed = vec![("src/a.ts".to_string(), "2".to_string())];
        assert_ne!(k1, action_key(&t, "linux", &src_changed, &[]).unwrap());
        assert_ne!(
            k1,
            action_key(&t, "linux", &files, &["dep".into()]).unwrap()
        );
        assert_ne!(k1, action_key(&t, "windows", &files, &[]).unwrap());
    }
}
