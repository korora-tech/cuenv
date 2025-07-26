package main

/*
#include <stdlib.h>
*/
import "C"
import (
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"
	"unsafe"

	"cuelang.org/go/cue"
	"cuelang.org/go/cue/cuecontext"
	"cuelang.org/go/cue/load"
	"cuelang.org/go/mod/modconfig"
)

// extractSecretReference checks if a CUE value has a resolver field, indicating it's a secret
func extractSecretReference(val cue.Value) string {
	// Check if this value has a resolver field (indicates it's a secret type)
	resolverField := val.LookupPath(cue.ParsePath("resolver"))
	if !resolverField.Exists() {
		return ""
	}

	// Extract the resolver configuration
	// Check for both "cmd" and "command" fields for compatibility
	cmdField := resolverField.LookupPath(cue.ParsePath("cmd"))
	if !cmdField.Exists() {
		cmdField = resolverField.LookupPath(cue.ParsePath("command"))
	}
	argsField := resolverField.LookupPath(cue.ParsePath("args"))

	if !cmdField.Exists() || !argsField.Exists() {
		return ""
	}

	// For now, we'll encode the resolver as a JSON string that the Rust side can decode
	// In the future, this could be a more sophisticated encoding
	type Resolver struct {
		Cmd  string   `json:"cmd"`
		Args []string `json:"args"`
	}

	var cmd string
	if err := cmdField.Decode(&cmd); err != nil {
		return ""
	}

	// Decode args array
	iter, _ := argsField.List()
	var args []string
	for iter.Next() {
		var arg string
		if err := iter.Value().Decode(&arg); err == nil {
			args = append(args, arg)
		}
	}

	resolver := Resolver{
		Cmd:  cmd,
		Args: args,
	}

	// Encode as JSON with a special prefix to identify it as a resolver
	jsonBytes, err := json.Marshal(resolver)
	if err != nil {
		return ""
	}

	return "cuenv-resolver://" + string(jsonBytes)
}

//export cue_free_string
func cue_free_string(s *C.char) {
	C.free(unsafe.Pointer(s))
}

//export cue_eval_package
func cue_eval_package(dirPath *C.char, packageName *C.char) *C.char {
	goDir := C.GoString(dirPath)
	goPkg := C.GoString(packageName)

	// Only allow loading the "env" package
	if goPkg != "env" {
		errMsg := map[string]string{"error": fmt.Sprintf("Only 'env' package is supported, got '%s'. Please ensure your .cue files use 'package env'", goPkg)}
		errBytes, _ := json.Marshal(errMsg)
		return C.CString(string(errBytes))
	}

	// Create a registry for module resolution
	registry, err := modconfig.NewRegistry(&modconfig.Config{
		Env: os.Environ(),
	})
	if err != nil {
		errMsg := map[string]string{"error": "Failed to create registry: " + err.Error()}
		errBytes, _ := json.Marshal(errMsg)
		return C.CString(string(errBytes))
	}

	// Load the CUE package from the directory
	// CUE will automatically search for module root in parent directories
	cfg := &load.Config{
		Dir:      goDir,
		Package:  goPkg,
		Registry: registry,
		Env:      os.Environ(),
	}

	// Load all .cue files in the directory
	instances := load.Instances(nil, cfg)
	if len(instances) == 0 {
		errMsg := map[string]string{"error": "No CUE instances found in directory"}
		errBytes, _ := json.Marshal(errMsg)
		return C.CString(string(errBytes))
	}

	// Check for load errors
	inst := instances[0]
	if inst.Err != nil {
		errMsg := map[string]string{"error": inst.Err.Error()}
		errBytes, _ := json.Marshal(errMsg)
		return C.CString(string(errBytes))
	}

	// Build the instance
	ctx := cuecontext.New()
	v := ctx.BuildInstance(inst)

	if v.Err() != nil {
		errMsg := map[string]string{"error": v.Err().Error()}
		errBytes, _ := json.Marshal(errMsg)
		return C.CString(string(errBytes))
	}

	// Use the same extraction logic as cue_parse_string
	result := extractCueData(v)

	// Convert to JSON
	jsonBytes, err := json.Marshal(result)
	if err != nil {
		errMsg := map[string]string{"error": err.Error()}
		errBytes, _ := json.Marshal(errMsg)
		return C.CString(string(errBytes))
	}

	return C.CString(string(jsonBytes))
}

// extractCueData extracts the structured data from a CUE value
func extractCueData(v cue.Value) map[string]interface{} {
	result := map[string]interface{}{
		"variables":    make(map[string]interface{}),
		"metadata":     make(map[string]interface{}),
		"environments": make(map[string]interface{}),
		"commands":     make(map[string]interface{}),
		"tasks":        make(map[string]interface{}),
		"hooks":        nil, // Initialize as nil, will be set if hooks exist
	}

	// Get metadata map reference for use throughout
	metadata := result["metadata"].(map[string]interface{})

	// Look for the 'env' field which contains the environment definition
	envRoot := v.LookupPath(cue.ParsePath("env"))
	if !envRoot.Exists() {
		// Return empty result if no env field
		return result
	}

	// Extract environment configurations if present
	if envField := envRoot.LookupPath(cue.ParsePath("environment")); envField.Exists() {
		envs := make(map[string]interface{})
		iter, _ := envField.Fields()
		for iter.Next() {
			envName := iter.Label()
			envVars := make(map[string]interface{})
			envMeta := make(map[string]interface{})
			envIter, _ := iter.Value().Fields()
			for envIter.Next() {
				key := envIter.Label()
				val := envIter.Value()

				// Extract attributes for environment-specific variables
				attrs := val.Attributes(cue.ValueAttr)
				varMeta := make(map[string]interface{})

				for _, attr := range attrs {
					if attr.Name() == "capability" {
						if caps, err := attr.String(0); err == nil {
							varMeta["capability"] = caps
							// Also store in parent metadata if not already there
							if _, exists := metadata[key]; !exists {
								metadata[key] = map[string]interface{}{"capability": caps}
							}
						}
					}
				}

				if len(varMeta) > 0 {
					envMeta[key] = varMeta
				}

				// Check if this is a secret type
				secretRef := extractSecretReference(val)
				if secretRef != "" {
					envVars[key] = secretRef
				} else {
					// Regular value
					var goVal interface{}
					if err := val.Decode(&goVal); err == nil {
						envVars[key] = goVal
					}
				}
			}
			envs[envName] = envVars
		}
		result["environments"] = envs
	}

	// Extract capabilities configuration if present (from env field)
	if capField := envRoot.LookupPath(cue.ParsePath("capabilities")); capField.Exists() {
		caps := make(map[string]interface{})
		iter, _ := capField.Fields()
		for iter.Next() {
			capName := iter.Label()
			capConfig := make(map[string]interface{})
			if cmdsField := iter.Value().LookupPath(cue.ParsePath("commands")); cmdsField.Exists() {
				var cmds []string
				if err := cmdsField.Decode(&cmds); err == nil {
					capConfig["commands"] = cmds
				}
			}
			caps[capName] = capConfig
		}
		result["capabilities"] = caps
	}

	// Also check for capabilities at the top level (outside env)
	if capField := v.LookupPath(cue.ParsePath("capabilities")); capField.Exists() {
		var caps map[string]interface{}
		if existing, ok := result["capabilities"].(map[string]interface{}); ok {
			caps = existing
		} else {
			caps = make(map[string]interface{})
		}
		iter, _ := capField.Fields()
		for iter.Next() {
			capName := iter.Label()
			capConfig := make(map[string]interface{})
			if cmdsField := iter.Value().LookupPath(cue.ParsePath("commands")); cmdsField.Exists() {
				var cmds []string
				if err := cmdsField.Decode(&cmds); err == nil {
					capConfig["commands"] = cmds
				}
			}
			caps[capName] = capConfig
		}
		result["capabilities"] = caps
	}

	// Build command-to-capabilities mapping from all collected capabilities
	if capsData, ok := result["capabilities"].(map[string]interface{}); ok && len(capsData) > 0 {
		commands := make(map[string]interface{})
		for capName, capConfig := range capsData {
			if capMap, ok := capConfig.(map[string]interface{}); ok {
				if cmds, ok := capMap["commands"].([]string); ok {
					for _, cmd := range cmds {
						if cmdConfig, exists := commands[cmd]; exists {
							// Command already exists, append capability
							if cmdMap, ok := cmdConfig.(map[string]interface{}); ok {
								if caps, ok := cmdMap["capabilities"].([]string); ok {
									cmdMap["capabilities"] = append(caps, capName)
								}
							}
						} else {
							// New command
							commands[cmd] = map[string]interface{}{
								"capabilities": []string{capName},
							}
						}
					}
				}
			}
		}

		// Sort capabilities for each command to ensure deterministic ordering
		for _, cmdConfig := range commands {
			if cmdMap, ok := cmdConfig.(map[string]interface{}); ok {
				if caps, ok := cmdMap["capabilities"].([]string); ok {
					sort.Strings(caps)
					cmdMap["capabilities"] = caps
				}
			}
		}

		result["commands"] = commands
	}

	// Extract tasks configuration if present (top-level only)
	if tasksField := v.LookupPath(cue.ParsePath("tasks")); tasksField.Exists() {
		tasks := make(map[string]interface{})
		iter, _ := tasksField.Fields()
		for iter.Next() {
			taskName := iter.Label()
			taskConfig := make(map[string]interface{})

			// Extract description
			if descField := iter.Value().LookupPath(cue.ParsePath("description")); descField.Exists() {
				var desc string
				if err := descField.Decode(&desc); err == nil {
					taskConfig["description"] = desc
				}
			}

			// Extract command
			if cmdField := iter.Value().LookupPath(cue.ParsePath("command")); cmdField.Exists() {
				var cmd string
				if err := cmdField.Decode(&cmd); err == nil {
					taskConfig["command"] = cmd
				}
			}

			// Extract script
			if scriptField := iter.Value().LookupPath(cue.ParsePath("script")); scriptField.Exists() {
				var script string
				if err := scriptField.Decode(&script); err == nil {
					taskConfig["script"] = script
				}
			}

			// Extract dependencies
			if depsField := iter.Value().LookupPath(cue.ParsePath("dependencies")); depsField.Exists() {
				var deps []string
				if err := depsField.Decode(&deps); err == nil {
					taskConfig["dependencies"] = deps
				}
			}

			// Extract workingDir
			if wdField := iter.Value().LookupPath(cue.ParsePath("workingDir")); wdField.Exists() {
				var wd string
				if err := wdField.Decode(&wd); err == nil {
					taskConfig["workingDir"] = wd
				}
			}

			// Extract shell
			if shellField := iter.Value().LookupPath(cue.ParsePath("shell")); shellField.Exists() {
				var shell string
				if err := shellField.Decode(&shell); err == nil {
					taskConfig["shell"] = shell
				}
			}

			// Extract inputs
			if inputsField := iter.Value().LookupPath(cue.ParsePath("inputs")); inputsField.Exists() {
				var inputs []string
				if err := inputsField.Decode(&inputs); err == nil {
					taskConfig["inputs"] = inputs
				}
			}

			// Extract outputs
			if outputsField := iter.Value().LookupPath(cue.ParsePath("outputs")); outputsField.Exists() {
				var outputs []string
				if err := outputsField.Decode(&outputs); err == nil {
					taskConfig["outputs"] = outputs
				}
			}

			// Extract security configuration
			if securityField := iter.Value().LookupPath(cue.ParsePath("security")); securityField.Exists() {
				security := make(map[string]interface{})

				// Extract restrictDisk
				if rdField := securityField.LookupPath(cue.ParsePath("restrictDisk")); rdField.Exists() {
					var restrictDisk bool
					if err := rdField.Decode(&restrictDisk); err == nil {
						security["restrictDisk"] = restrictDisk
					}
				}

				// Extract restrictNetwork
				if rnField := securityField.LookupPath(cue.ParsePath("restrictNetwork")); rnField.Exists() {
					var restrictNetwork bool
					if err := rnField.Decode(&restrictNetwork); err == nil {
						security["restrictNetwork"] = restrictNetwork
					}
				}

				// Extract readOnlyPaths
				if roField := securityField.LookupPath(cue.ParsePath("readOnlyPaths")); roField.Exists() {
					var readOnlyPaths []string
					if err := roField.Decode(&readOnlyPaths); err == nil {
						security["readOnlyPaths"] = readOnlyPaths
					}
				}

				// Extract readWritePaths
				if rwField := securityField.LookupPath(cue.ParsePath("readWritePaths")); rwField.Exists() {
					var readWritePaths []string
					if err := rwField.Decode(&readWritePaths); err == nil {
						security["readWritePaths"] = readWritePaths
					}
				}

				// Extract allowedHosts
				if ahField := securityField.LookupPath(cue.ParsePath("allowedHosts")); ahField.Exists() {
					var allowedHosts []string
					if err := ahField.Decode(&allowedHosts); err == nil {
						security["allowedHosts"] = allowedHosts
					}
				}

				// Extract allowNew
				if anField := securityField.LookupPath(cue.ParsePath("allowNew")); anField.Exists() {
					var allowNew bool
					if err := anField.Decode(&allowNew); err == nil {
						security["allowNew"] = allowNew
					}
				}

				taskConfig["security"] = security
			}

			// Extract cache
			if cacheField := iter.Value().LookupPath(cue.ParsePath("cache")); cacheField.Exists() {
				var cache bool
				if err := cacheField.Decode(&cache); err == nil {
					taskConfig["cache"] = cache
				}
			}

			// Extract cacheKey
			if cacheKeyField := iter.Value().LookupPath(cue.ParsePath("cacheKey")); cacheKeyField.Exists() {
				var cacheKey string
				if err := cacheKeyField.Decode(&cacheKey); err == nil {
					taskConfig["cacheKey"] = cacheKey
				}
			}

			tasks[taskName] = taskConfig
		}
		result["tasks"] = tasks
	}

	// Extract hooks configuration if present (at root level, not under env)
	if hooksField := v.LookupPath(cue.ParsePath("hooks")); hooksField.Exists() {
		hooks := make(map[string]interface{})

		// Extract onEnter hook
		if onEnterField := hooksField.LookupPath(cue.ParsePath("onEnter")); onEnterField.Exists() {
			onEnter := make(map[string]interface{})
			if cmdField := onEnterField.LookupPath(cue.ParsePath("command")); cmdField.Exists() {
				var cmd string
				if err := cmdField.Decode(&cmd); err == nil {
					onEnter["command"] = cmd
				}
			}
			if argsField := onEnterField.LookupPath(cue.ParsePath("args")); argsField.Exists() {
				var args []string
				if err := argsField.Decode(&args); err == nil {
					onEnter["args"] = args
				}
			}
			if urlField := onEnterField.LookupPath(cue.ParsePath("url")); urlField.Exists() {
				var url string
				if err := urlField.Decode(&url); err == nil {
					onEnter["url"] = url
				}
			}
			// Extract constraints
			if constraintsField := onEnterField.LookupPath(cue.ParsePath("constraints")); constraintsField.Exists() {
				var constraints []interface{}
				if err := constraintsField.Decode(&constraints); err == nil {
					onEnter["constraints"] = constraints
				}
			}
			if len(onEnter) > 0 {
				hooks["onEnter"] = onEnter
			}
		}

		// Extract onExit hook
		if onExitField := hooksField.LookupPath(cue.ParsePath("onExit")); onExitField.Exists() {
			onExit := make(map[string]interface{})
			if cmdField := onExitField.LookupPath(cue.ParsePath("command")); cmdField.Exists() {
				var cmd string
				if err := cmdField.Decode(&cmd); err == nil {
					onExit["command"] = cmd
				}
			}
			if argsField := onExitField.LookupPath(cue.ParsePath("args")); argsField.Exists() {
				var args []string
				if err := argsField.Decode(&args); err == nil {
					onExit["args"] = args
				}
			}
			if urlField := onExitField.LookupPath(cue.ParsePath("url")); urlField.Exists() {
				var url string
				if err := urlField.Decode(&url); err == nil {
					onExit["url"] = url
				}
			}
			// Extract constraints
			if constraintsField := onExitField.LookupPath(cue.ParsePath("constraints")); constraintsField.Exists() {
				var constraints []interface{}
				if err := constraintsField.Decode(&constraints); err == nil {
					onExit["constraints"] = constraints
				}
			}
			if len(onExit) > 0 {
				hooks["onExit"] = onExit
			}
		}

		if len(hooks) > 0 {
			result["hooks"] = hooks
		}
	}

	// Extract variables with capability metadata
	vars := result["variables"].(map[string]interface{})

	iter, _ := envRoot.Fields()
	for iter.Next() {
		key := iter.Label()
		val := iter.Value()

		// Skip internal CUE fields, private fields, and special keys
		if strings.HasPrefix(key, "_") || strings.HasPrefix(key, "#") ||
			key == "environment" || key == "capabilities" || key == "hooks" || key == "tasks" {
			continue
		}

		// Extract attributes (like @capability)
		attrs := val.Attributes(cue.ValueAttr)
		varMeta := make(map[string]interface{})

		for _, attr := range attrs {
			if attr.Name() == "capability" {
				if caps, err := attr.String(0); err == nil {
					varMeta["capability"] = caps
				}
			}
		}

		// Check if this is a secret type and convert accordingly
		secretRef := extractSecretReference(val)
		if secretRef != "" {
			vars[key] = secretRef
			if len(varMeta) > 0 {
				metadata[key] = varMeta
			}
		} else {
			// Convert CUE value to Go value
			var goVal interface{}
			if err := val.Decode(&goVal); err == nil {
				vars[key] = goVal
				if len(varMeta) > 0 {
					metadata[key] = varMeta
				}
			}
		}
	}

	return result
}

func main() {}
