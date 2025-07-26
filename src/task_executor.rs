use crate::cue_parser::TaskConfig;
use crate::env_manager::EnvManager;
use crate::errors::{Error, Result};
use crate::task_cache::TaskCache;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use tokio::task::JoinSet;

/// Represents a task execution plan with resolved dependencies
#[derive(Debug, Clone)]
pub struct TaskExecutionPlan {
    /// Tasks organized by execution level (level 0 = no dependencies, etc.)
    pub levels: Vec<Vec<String>>,
    /// Original task configurations
    pub tasks: HashMap<String, TaskConfig>,
}

/// Main task executor that handles dependency resolution and execution
pub struct TaskExecutor {
    env_manager: EnvManager,
    working_dir: PathBuf,
}

impl TaskExecutor {
    /// Create a new task executor
    pub fn new(env_manager: EnvManager, working_dir: PathBuf) -> Result<Self> {
        Ok(Self {
            env_manager,
            working_dir,
        })
    }

    /// Execute a single task by name
    pub async fn execute_task(&self, task_name: &str, args: &[String]) -> Result<i32> {
        self.execute_tasks_with_dependencies(&[task_name.to_string()], args, false)
            .await
    }

    /// Execute a single task by name with audit mode
    pub async fn execute_task_with_audit(&self, task_name: &str, args: &[String]) -> Result<i32> {
        self.execute_tasks_with_dependencies(&[task_name.to_string()], args, true)
            .await
    }

    /// Execute multiple tasks with their dependencies
    pub async fn execute_tasks_with_dependencies(
        &self,
        task_names: &[String],
        args: &[String],
        audit_mode: bool,
    ) -> Result<i32> {
        // Build execution plan
        let plan = self.build_execution_plan(task_names)?;

        // Execute tasks level by level
        for level in &plan.levels {
            let mut join_set = JoinSet::new();
            let failed_tasks = Arc::new(Mutex::new(Vec::new()));

            // Launch all tasks in this level concurrently
            for task_name in level {
                let task_config = plan.tasks.get(task_name).unwrap().clone();
                let working_dir = self.working_dir.clone();
                let task_args = args.to_vec();
                let failed_tasks = Arc::clone(&failed_tasks);
                let task_name = task_name.clone();
                let task_cache = TaskCache::new()?; // Create cache instance for this task

                join_set.spawn(async move {
                    match Self::execute_single_task_with_cache(
                        &task_name,
                        &task_config,
                        &working_dir,
                        &task_args,
                        &task_cache,
                        audit_mode,
                    )
                    .await
                    {
                        Ok(status) => {
                            if status != 0 {
                                failed_tasks
                                    .lock()
                                    .unwrap()
                                    .push((task_name.clone(), status));
                            }
                            status
                        }
                        Err(e) => {
                            failed_tasks.lock().unwrap().push((task_name.clone(), -1));
                            eprintln!("Task '{task_name}' failed: {e}");
                            -1
                        }
                    }
                });
            }

            // Wait for all tasks in this level to complete
            while let Some(result) = join_set.join_next().await {
                if let Err(e) = result {
                    return Err(Error::configuration(format!("Task execution failed: {e}")));
                }
            }

            // Check if any tasks failed
            let failed = failed_tasks.lock().unwrap();
            if !failed.is_empty() {
                let failed_names: Vec<String> =
                    failed.iter().map(|(name, _)| name.clone()).collect();
                return Err(Error::configuration(format!(
                    "Tasks failed: {}",
                    failed_names.join(", ")
                )));
            }
        }

        Ok(0)
    }

    /// Build an execution plan with dependency resolution
    pub fn build_execution_plan(&self, task_names: &[String]) -> Result<TaskExecutionPlan> {
        let all_tasks = self.env_manager.get_tasks();

        // Validate that all requested tasks exist
        for task_name in task_names {
            if !all_tasks.contains_key(task_name) {
                return Err(Error::configuration(format!(
                    "Task '{task_name}' not found"
                )));
            }
        }

        // Build dependency graph
        let mut task_dependencies = HashMap::new();
        let mut visited = HashSet::new();
        let mut stack = HashSet::new();

        for task_name in task_names {
            Self::collect_dependencies(
                task_name,
                all_tasks,
                &mut task_dependencies,
                &mut visited,
                &mut stack,
            )?;
        }

        // Topological sort to determine execution order
        let levels = self.topological_sort(&task_dependencies)?;

        // Build final execution plan
        let mut plan_tasks = HashMap::new();
        for task_name in task_dependencies.keys() {
            if let Some(config) = all_tasks.get(task_name) {
                plan_tasks.insert(task_name.clone(), config.clone());
            }
        }

        Ok(TaskExecutionPlan {
            levels,
            tasks: plan_tasks,
        })
    }

    /// Recursively collect all dependencies for a task
    fn collect_dependencies(
        task_name: &str,
        all_tasks: &HashMap<String, TaskConfig>,
        task_dependencies: &mut HashMap<String, Vec<String>>,
        visited: &mut HashSet<String>,
        stack: &mut HashSet<String>,
    ) -> Result<()> {
        // Check for circular dependencies
        if stack.contains(task_name) {
            return Err(Error::configuration(format!(
                "Circular dependency detected involving task '{task_name}'"
            )));
        }

        if visited.contains(task_name) {
            return Ok(());
        }

        stack.insert(task_name.to_string());

        let task_config = all_tasks
            .get(task_name)
            .ok_or_else(|| Error::configuration(format!("Task '{task_name}' not found")))?;

        let dependencies = task_config.dependencies.clone().unwrap_or_default();

        // Validate and collect dependencies
        for dep_name in &dependencies {
            if !all_tasks.contains_key(dep_name) {
                return Err(Error::configuration(format!(
                    "Dependency '{dep_name}' of task '{task_name}' not found"
                )));
            }

            Self::collect_dependencies(dep_name, all_tasks, task_dependencies, visited, stack)?;
        }

        task_dependencies.insert(task_name.to_string(), dependencies);
        visited.insert(task_name.to_string());
        stack.remove(task_name);

        Ok(())
    }

    /// Perform topological sort to determine execution levels
    fn topological_sort(
        &self,
        dependencies: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<Vec<String>>> {
        let mut in_degree = HashMap::new();
        let mut graph = HashMap::new();

        // Initialize in-degree count and adjacency list
        for (task, deps) in dependencies {
            in_degree.entry(task.clone()).or_insert(0);
            graph.entry(task.clone()).or_insert(Vec::new());

            for dep in deps {
                *in_degree.entry(dep.clone()).or_insert(0) += 0; // Ensure dep is in map
                graph
                    .entry(dep.clone())
                    .or_insert_with(Vec::new)
                    .push(task.clone());
                *in_degree.get_mut(task).unwrap() += 1;
            }
        }

        let mut levels = Vec::new();
        let mut queue: VecDeque<String> = in_degree
            .iter()
            .filter(|(_, &degree)| degree == 0)
            .map(|(task, _)| task.clone())
            .collect();

        while !queue.is_empty() {
            let current_level: Vec<String> = queue.drain(..).collect();

            if current_level.is_empty() {
                break;
            }

            for task in &current_level {
                if let Some(dependents) = graph.get(task) {
                    for dependent in dependents {
                        if let Some(degree) = in_degree.get_mut(dependent) {
                            *degree -= 1;
                            if *degree == 0 {
                                queue.push_back(dependent.clone());
                            }
                        }
                    }
                }
            }

            levels.push(current_level);
        }

        // Check for remaining tasks (would indicate circular dependencies)
        let processed_count: usize = levels.iter().map(|level| level.len()).sum();
        if processed_count != dependencies.len() {
            return Err(Error::configuration(
                "Circular dependency detected in task graph".to_string(),
            ));
        }

        Ok(levels)
    }

    /// Execute a single task with caching support
    async fn execute_single_task_with_cache(
        task_name: &str,
        task_config: &TaskConfig,
        working_dir: &Path,
        args: &[String],
        task_cache: &TaskCache,
    ) -> Result<i32> {
        // Generate cache key
        let cache_key = task_cache.generate_cache_key(task_name, task_config, working_dir)?;

        // Check if task result is cached
        if let Some(cached_result) = task_cache.get_cached_result(&cache_key, task_config, working_dir)? {
            println!("✓ Task '{}' found in cache, skipping execution", task_name);
            return Ok(cached_result.exit_code);
        }

        // Execute the task
        println!("→ Executing task '{}'", task_name);
        let exit_code = Self::execute_single_task(task_config, working_dir, args).await?;

        // Cache the result
        if let Err(e) = task_cache.save_result(&cache_key, task_config, working_dir, exit_code) {
            log::warn!("Failed to cache task '{}' result: {}", task_name, e);
        }

        Ok(exit_code)
    }

    /// Execute a single task
    async fn execute_single_task(
        task_config: &TaskConfig,
        working_dir: &Path,
        args: &[String],
        audit_mode: bool,
    ) -> Result<i32> {
        // Determine what to execute
        let (shell, script_content) = match (&task_config.command, &task_config.script) {
            (Some(command), None) => {
                // Add user args to the command
                let full_command = if args.is_empty() {
                    command.clone()
                } else {
                    format!("{} {}", command, args.join(" "))
                };
                (
                    task_config
                        .shell
                        .clone()
                        .unwrap_or_else(|| "sh".to_string()),
                    full_command,
                )
            }
            (None, Some(script)) => (
                task_config
                    .shell
                    .clone()
                    .unwrap_or_else(|| "sh".to_string()),
                script.clone(),
            ),
            (Some(_), Some(_)) => {
                return Err(Error::configuration(
                    "Task cannot have both 'command' and 'script' defined".to_string(),
                ));
            }
            (None, None) => {
                return Err(Error::configuration(
                    "Task must have either 'command' or 'script' defined".to_string(),
                ));
            }
        };

        // Determine working directory
        let exec_dir = if let Some(task_wd) = &task_config.working_dir {
            let mut dir = working_dir.to_path_buf();
            dir.push(task_wd);
            dir
        } else {
            working_dir.to_path_buf()
        };

        // Execute the task
        let mut cmd = Command::new(&shell);
        cmd.arg("-c")
            .arg(&script_content)
            .current_dir(&exec_dir)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        // Apply security restrictions if configured
        if let Some(security) = &task_config.security {
            use crate::access_restrictions::AccessRestrictions;
            let mut restrictions =
                AccessRestrictions::from_security_config_with_task(security, task_config);

            if audit_mode {
                restrictions.enable_audit_mode();
                println!("🔍 Running task in audit mode...");

                let (exit_code, audit_report) = restrictions.run_with_audit(&mut cmd)?;
                audit_report.print_summary();
                return Ok(exit_code);
            } else if restrictions.has_any_restrictions() {
                restrictions.apply_to_command(&mut cmd)?;
            }
        }

        let output = cmd.output().map_err(|e| {
            Error::command_execution(
                &shell,
                vec!["-c".to_string(), script_content.clone()],
                format!("Failed to execute task: {e}"),
                None,
            )
        })?;

        Ok(output.status.code().unwrap_or(1))
    }

    /// List all available tasks
    pub fn list_tasks(&self) -> Vec<(String, Option<String>)> {
        self.env_manager.list_tasks()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_env_manager_with_tasks(tasks_cue: &str) -> (EnvManager, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let env_file = temp_dir.path().join("env.cue");
        fs::write(&env_file, tasks_cue).unwrap();

        let mut manager = EnvManager::new();
        manager.load_env(temp_dir.path()).unwrap();
        (manager, temp_dir)
    }

    #[test]
    fn test_simple_task_discovery() {
        let tasks_cue = r#"package env

env: {
    DATABASE_URL: "test"
}

tasks: {
    "build": {
        description: "Build the project"
        command: "echo 'Building...'"
    }
    "test": {
        description: "Run tests"
        command: "echo 'Testing...'"
    }
}"#;

        let (manager, _temp_dir) = create_test_env_manager_with_tasks(tasks_cue);
        let executor = TaskExecutor::new(manager, PathBuf::from(".")).unwrap();

        let tasks = executor.list_tasks();
        assert_eq!(tasks.len(), 2);

        let task_names: Vec<&String> = tasks.iter().map(|(name, _)| name).collect();
        assert!(task_names.contains(&&"build".to_string()));
        assert!(task_names.contains(&&"test".to_string()));
    }

    #[test]
    fn test_task_dependency_resolution() {
        let tasks_cue = r#"package env

env: {}

tasks: {
    "build": {
        description: "Build the project"
        command: "echo 'Building...'"
        dependencies: ["test"]
    }
    "test": {
        description: "Run tests"
        command: "echo 'Testing...'"
    }
}"#;

        let (manager, _temp_dir) = create_test_env_manager_with_tasks(tasks_cue);
        let executor = TaskExecutor::new(manager, PathBuf::from(".")).unwrap();

        let plan = executor
            .build_execution_plan(&["build".to_string()])
            .unwrap();

        // Should have 2 levels: [test], [build]
        assert_eq!(plan.levels.len(), 2);
        assert_eq!(plan.levels[0], vec!["test"]);
        assert_eq!(plan.levels[1], vec!["build"]);
    }

    #[test]
    fn test_circular_dependency_detection() {
        let tasks_cue = r#"package env

env: {}

tasks: {
    "task1": {
        command: "echo 'Task 1'"
        dependencies: ["task2"]
    }
    "task2": {
        command: "echo 'Task 2'"
        dependencies: ["task1"]
    }
}"#;

        let (manager, _temp_dir) = create_test_env_manager_with_tasks(tasks_cue);
        let executor = TaskExecutor::new(manager, PathBuf::from(".")).unwrap();

        let result = executor.build_execution_plan(&["task1".to_string()]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Circular dependency"));
    }

    #[test]
    fn test_missing_task_error() {
        let tasks_cue = r#"package env

env: {}

tasks: {
    "build": {
        command: "echo 'Building...'"
    }
}"#;

        let (manager, _temp_dir) = create_test_env_manager_with_tasks(tasks_cue);
        let executor = TaskExecutor::new(manager, PathBuf::from(".")).unwrap();

        let result = executor.build_execution_plan(&["nonexistent".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_missing_dependency_error() {
        let tasks_cue = r#"package env

env: {}

tasks: {
    "build": {
        command: "echo 'Building...'"
        dependencies: ["nonexistent"]
    }
}"#;

        let (manager, _temp_dir) = create_test_env_manager_with_tasks(tasks_cue);
        let executor = TaskExecutor::new(manager, PathBuf::from(".")).unwrap();

        let result = executor.build_execution_plan(&["build".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_complex_dependency_graph() {
        let tasks_cue = r#"package env

env: {}

tasks: {
    "deploy": {
        command: "echo 'Deploying...'"
        dependencies: ["build", "test"]
    }
    "build": {
        command: "echo 'Building...'"
        dependencies: ["compile"]
    }
    "test": {
        command: "echo 'Testing...'"
        dependencies: ["compile"]
    }
    "compile": {
        command: "echo 'Compiling...'"
    }
}"#;

        let (manager, _temp_dir) = create_test_env_manager_with_tasks(tasks_cue);
        let executor = TaskExecutor::new(manager, PathBuf::from(".")).unwrap();

        let plan = executor
            .build_execution_plan(&["deploy".to_string()])
            .unwrap();

        // Should have 3 levels: [compile], [build, test], [deploy]
        assert_eq!(plan.levels.len(), 3);
        assert_eq!(plan.levels[0], vec!["compile"]);
        assert_eq!(plan.levels[1].len(), 2);
        assert!(plan.levels[1].contains(&"build".to_string()));
        assert!(plan.levels[1].contains(&"test".to_string()));
        assert_eq!(plan.levels[2], vec!["deploy"]);
    }
}
