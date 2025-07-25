# Tasks Example with Build Cache

This example demonstrates cuenv's powerful task system with the new Bazel-style build cache support.

## Features Demonstrated

- **Task Definition**: Define tasks with descriptions, commands/scripts, and dependencies
- **Build Cache**: Bazel-style caching with content-based cache keys and automatic invalidation
- **Dependency Resolution**: Tasks automatically execute their dependencies in the correct order
- **Parallel Execution**: Independent tasks run concurrently for better performance
- **Environment Inheritance**: Tasks inherit all environment variables from env.cue
- **Input/Output Tracking**: Track file dependencies for intelligent cache invalidation

## Build Cache Features

The build cache system provides enterprise-grade caching with:

- **Content-based Cache Keys**: SHA256 hashing of task configuration + input file contents
- **Input Tracking**: Automatic cache invalidation when input files change
- **Output Validation**: Cached tasks verified by checking output file existence and hashes
- **Dependency-aware**: Works seamlessly with task dependency resolution
- **Custom Cache Keys**: Support for user-defined cache keys with `cacheKey` field
- **Selective Caching**: Tasks can disable caching with `cache: false`

## Usage

### List Available Tasks

```bash
cuenv run
```

### Execute a Task (with caching)

```bash
cuenv run lint        # First run executes
cuenv run lint        # Second run uses cache
```

### Execute Task with Dependencies

```bash
cuenv run build      # Executes lint → test → build (if not cached)
```

### Clear Build Cache

```bash
cuenv clear-cache
```

## Task Definition Schema

```cue
tasks: {
    "task-name": {
        description: "Human readable description"
        command: "shell command to execute"           // OR
        script: "multi-line shell script"             // (mutually exclusive)
        dependencies: ["other-task1", "other-task2"] // Optional
        workingDir: "./relative/path"                 // Optional
        shell: "bash"                                 // Optional (default: "sh")
        cache: true                                   // Enable build cache
        inputs: ["src/**/*.go", "tests/**/*"]         // Files to track for cache invalidation
        outputs: ["bin/myapp", "build/artifacts/*"]   // Output files for cache validation
        cacheKey: "custom-key"                        // Optional custom cache key
    }
}
```

## Example Tasks in this Directory

- **lint**: Lints the code (cached, tracks `src/*`)
- **test**: Runs tests (cached, depends on lint, tracks `src/*` and `tests/*`)
- **build**: Builds the project (cached, depends on test, outputs to `build/app`)
- **deploy**: Deploys the application (not cached, depends on build)
- **clean**: Cleans build artifacts (not cached, always runs)
- **script-example**: Demonstrates multi-line script with caching

## Cache Behavior Examples

### Initial Run
```bash
$ cuenv run build
→ Executing task 'lint'
Linting code...
→ Executing task 'test'  
Running tests...
→ Executing task 'build'
Building project...
```

### Subsequent Run (All Cached)
```bash
$ cuenv run build
✓ Task 'lint' found in cache, skipping execution
✓ Task 'test' found in cache, skipping execution  
✓ Task 'build' found in cache, skipping execution
```

### After Modifying Input File
```bash
$ echo "// comment" >> src/main.rs
$ cuenv run build
→ Executing task 'lint'       # Cache invalidated due to input change
Linting code...
✓ Task 'test' found in cache, skipping execution  # Different inputs
→ Executing task 'build'      # Cache invalidated due to dependency change
Building project...
```

## Cache Storage

- **Location**: `~/.cache/cuenv/tasks/`
- **Format**: JSON files with SHA256 filenames
- **Contents**: Exit code, output file hashes, execution timestamp
- **Cleanup**: Use `cuenv clear-cache` to remove all cached results

## Error Handling

- **Missing Task**: Error if requested task doesn't exist
- **Missing Dependency**: Error if task depends on non-existent task
- **Circular Dependencies**: Detected and reported as error
- **Task Failure**: Failed tasks are not cached, stops execution chain
- **Cache Corruption**: Automatic cache invalidation on file corruption

## Environment Variables

All tasks automatically inherit environment variables defined in the env.cue file:

- `DATABASE_URL`: postgres://localhost/myapp
- `API_KEY`: test-api-key
- `PORT`: 3000

These are available as `$DATABASE_URL`, `$API_KEY`, `$PORT` in task commands/scripts.

## Performance Benefits

The build cache dramatically improves development workflows:

- **Skip redundant compilation**: Only rebuild when source files change
- **Faster CI/CD**: Reuse artifacts across pipeline stages
- **Incremental builds**: Smart dependency tracking
- **Team collaboration**: Shared cache improves team productivity

This makes cuenv ideal for monorepos and complex build pipelines where build performance is critical.
