# Hermes-RS: 12-Factor App Implementation

This document explains how Hermes-RS implements the [12-Factor App](https://12factor.net/) methodology for building cloud-native applications.

## ✅ 12-Factor Compliance

### I. Codebase
- **✅ Implemented**: Single codebase tracked in Git with multiple deployments
- Git repository serves as single source of truth
- Different environments use same codebase with different configs

### II. Dependencies
- **✅ Implemented**: Dependencies explicitly declared in `Cargo.toml`
- Rust's cargo system ensures reproducible builds
- No system-wide dependencies assumed

### III. Config
- **✅ Implemented**: Configuration stored in environment variables
- Use `.env` file for local development
- All config options available as CLI args or env vars
- Example: `HERMES_PORT`, `HERMES_LOG_LEVEL`, `HERMES_CONFIG_PATH`

### IV. Backing Services
- **✅ Implemented**: Backing services treated as attached resources
- HTTP targets configured via URLs
- Ready for database, cache, and message queue integration
- Example: `DATABASE_URL`, `REDIS_URL` environment variables

### V. Build, Release, Run
- **✅ Implemented**: Strict separation of build and run stages
- Multi-stage Docker build
- GitHub Actions for automated builds
- Immutable releases with version tags

### VI. Processes
- **✅ Implemented**: Stateless processes
- No local state stored in memory between requests
- All state externalized to backing services

### VII. Port Binding
- **✅ Implemented**: Self-contained web server
- Exports HTTP service via port binding
- Configurable bind address and port
- No external web server required

### VIII. Concurrency
- **✅ Implemented**: Async/await concurrency model
- Tokio runtime for handling concurrent requests
- Configurable concurrent request limits
- Horizontal scaling ready

### IX. Disposability
- **✅ Implemented**: Fast startup and graceful shutdown
- Signal handling for SIGTERM/SIGINT
- Graceful connection draining
- Quick startup time

### X. Dev/Prod Parity
- **✅ Implemented**: Development and production environments kept similar
- Same Docker image for all environments
- Environment-specific configuration only
- Consistent tooling and dependencies

### XI. Logs
- **✅ Implemented**: Logs treated as event streams
- Structured logging with JSON output option
- Logs written to stdout/stderr
- No log file management in application

### XII. Admin Processes
- **✅ Implemented**: Admin tasks as one-off processes
- `hermes-admin` binary for administrative tasks
- Configuration validation
- Template testing utilities

## Usage Examples

### Environment Configuration
```bash
# Set via environment variables
export HERMES_PORT=8080
export HERMES_LOG_LEVEL=debug
export HERMES_LOG_FORMAT=json

# Or use .env file
cp .env.example .env
# Edit .env with your values
```

### Running with Docker
```bash
# Build 12-factor compliant image
docker build -f Dockerfile.12factor -t hermes-rs:12factor .

# Run with environment config
docker run -p 3000:3000 \
  -e HERMES_LOG_FORMAT=json \
  -e HERMES_LOG_LEVEL=info \
  -v $(pwd)/config.yml:/app/config.yml:ro \
  hermes-rs:12factor
```

### Docker Compose
```bash
# Use 12-factor docker-compose setup
docker-compose -f docker-compose.12factor.yml up
```

### Admin Tasks
```bash
# Validate configuration
cargo run --bin hermes-admin validate-config

# Test template rendering
cargo run --bin hermes-admin test-template \
  --endpoint /webhook/github \
  --payload '{"action":"opened","repository":{"name":"test"}}'

# List all endpoints
cargo run --bin hermes-admin list-endpoints
```

### Health Checks
```bash
# Health check
curl http://localhost:3000/health

# Readiness check
curl http://localhost:3000/ready
```

### Logging Configuration
```bash
# Pretty logs for development
HERMES_LOG_FORMAT=pretty cargo run

# JSON logs for production
HERMES_LOG_FORMAT=json cargo run

# Debug level logging
HERMES_LOG_LEVEL=debug cargo run
```

## Production Deployment

### Kubernetes Example
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: hermes-rs
spec:
  replicas: 3
  selector:
    matchLabels:
      app: hermes-rs
  template:
    metadata:
      labels:
        app: hermes-rs
    spec:
      containers:
      - name: hermes-rs
        image: hermes-rs:12factor
        ports:
        - containerPort: 3000
        env:
        - name: HERMES_LOG_FORMAT
          value: "json"
        - name: HERMES_LOG_LEVEL
          value: "info"
        - name: DATABASE_URL
          valueFrom:
            secretKeyRef:
              name: hermes-secrets
              key: database-url
        livenessProbe:
          httpGet:
            path: /health
            port: 3000
        readinessProbe:
          httpGet:
            path: /ready
            port: 3000
        resources:
          requests:
            memory: "64Mi"
            cpu: "250m"
          limits:
            memory: "128Mi"
            cpu: "500m"
```

### Environment Variables Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `HERMES_BIND_ADDRESS` | `0.0.0.0` | Server bind address |
| `HERMES_PORT` | `3000` | Server port |
| `HERMES_CONFIG_PATH` | `config.yml` | Configuration file path |
| `HERMES_LOG_LEVEL` | `info` | Log level (trace, debug, info, warn, error) |
| `HERMES_LOG_FORMAT` | `pretty` | Log format (pretty, json) |
| `HERMES_REQUEST_TIMEOUT` | `30` | HTTP request timeout in seconds |
| `HERMES_MAX_CONCURRENT_REQUESTS` | `1000` | Maximum concurrent requests |
| `HERMES_HEALTH_CHECK_ENABLED` | `true` | Enable health check endpoints |

## Benefits of 12-Factor Implementation

1. **Cloud-Native**: Ready for deployment on any cloud platform
2. **Scalable**: Horizontal scaling without code changes
3. **Maintainable**: Clear separation of concerns
4. **Observable**: Structured logging and health checks
5. **Reliable**: Graceful shutdown and error handling
6. **Secure**: Non-root user, minimal attack surface
7. **Portable**: Consistent behavior across environments