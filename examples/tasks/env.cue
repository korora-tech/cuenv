package env

env: {
	// Environment variables
	DATABASE_URL: "postgres://localhost/myapp"
	API_KEY:      "test-api-key"
	PORT:         "3000"
}

// Task definitions at top level
tasks: {
	"build": {
		description: "Build the project"
		command:     "echo 'Building project...' && sleep 1 && echo 'Build complete!'"
		dependencies: ["test"]
		cache:     true
		inputs:    ["src/*"]
		outputs:   ["build/app"]
	}
	"test": {
		description: "Run tests"
		command:     "echo 'Running tests...' && sleep 1 && echo 'Tests passed!'"
		dependencies: ["lint"]
		cache:     true
		inputs:    ["src/*", "tests/*"]
	}
	"lint": {
		description: "Lint the code"
		command:     "echo 'Linting code...' && sleep 1 && echo 'Linting complete!'"
		cache:     true
		inputs:    ["src/*"]
	}
	"deploy": {
		description: "Deploy the application"
		command:     "echo 'Deploying to production...' && echo 'Using PORT:' $PORT && sleep 2 && echo 'Deployment complete!'"
		dependencies: ["build"]
		cache:     false  // Deployments should not be cached
	}
	"clean": {
		description: "Clean build artifacts"
		command:     "echo 'Cleaning build artifacts...'"
		cache:     false  // Clean should always run
	}
	"script-example": {
		description: "Example using script instead of command"
		cache:     true
		inputs:    ["src/*"]
		outputs:   ["build/script-output.txt"]
		script: """
			echo "This is a multi-line script"
			echo "Environment variables available:"
			echo "DATABASE_URL: $DATABASE_URL"
			echo "API_KEY: $API_KEY"
			echo "PORT: $PORT"
			mkdir -p build
			echo "Script output" > build/script-output.txt
			"""
	}
}
