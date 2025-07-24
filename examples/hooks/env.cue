package env

// Hook definitions at root level
hooks: {
    // Hook that runs when entering the environment
    onEnter: {
        command: "echo"
        args: ["🚀 Environment activated! Database: $DATABASE_URL"]
    }
    
    // Hook that runs when exiting the environment
    onExit: {
        command: "echo"
        args: ["👋 Cleaning up environment..."]
    }
}

// Regular environment variables
DATABASE_URL: "postgres://localhost/mydb"
API_KEY: "secret123"