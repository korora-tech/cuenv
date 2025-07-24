package env

import "github.com/rawkode/cuenv"

// Environment configuration
env: cuenv.#Env & {
	// Basic environment variables
	DATABASE_URL: "postgres://localhost/mydb"
	API_KEY:      "test-api-key"
	DEBUG:        "true"
	PORT:         "3000"

	// Using CUE's string interpolation
	BASE_URL:      "https://api.example.com"
	API_ENDPOINT:  "\(BASE_URL)/v1"

	// Computed values
	HOST:         "localhost"
	DATABASE_DSN: "postgres://\(HOST):5432/myapp"
}
