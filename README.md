# Rusty Static Server

A simple, multi-threaded static file HTTP/1.1 server written in Rust. Designed to be relatively robust and handle common edge cases.

## Features

* **Multi-threaded:** Uses a configurable thread pool to handle multiple client connections concurrently.
* **Static File Serving:** Serves files from a specified root directory.
* **MIME Type Detection:** Automatically detects and sends the correct `Content-Type` header based on file extension using the `mime_guess` crate.
* **Large File Streaming:** Streams large files in chunks to avoid high memory usage.
* **HEAD Request Support:** Handles `HEAD` requests efficiently by sending only headers.
* **Directory Index:** Serves `index.html` automatically when a directory path is requested.
* **Security:**
    * **Path Sanitization:** Uses `path-clean` and URL decoding to normalize paths and prevent directory traversal attacks (`../`, encoded variations).
    * **Root Directory Confinement:** Ensures requested files resolve within the specified root directory, preventing access to files outside the intended scope (checks canonical paths).
    * **Read Timeouts:** Configurable timeout for reading initial client requests to mitigate slow client attacks.
* **Custom Error Pages:** Supports serving custom `404.html` and `403.html` pages if they exist in the root directory.
* **Logging:** Logs requests, errors, and server status to both the console and a configurable log file (`server.log` by default). Includes timestamps and client IP addresses.
* **Configuration:** Configurable via command-line arguments (host, port, root directory, thread pool size, log file, read timeout).

## Prerequisites

* **Rust and Cargo:** You need to have the Rust programming language and its build tool/package manager, Cargo, installed. You can get them from [https://www.rust-lang.org/tools/install](https://www.rust-lang.org/tools/install).

## Dependencies

This project relies on the following external crates (managed by Cargo):

* `chrono`: For timestamping log messages.
* `mime_guess`: For guessing file MIME types.
* `num_cpus`: To determine the default number of worker threads based on available CPU cores.
* `path-clean`: For robust path normalization and sanitization.
* `urlencoding`: For decoding URL-encoded path segments.

Make sure these are listed in your `Cargo.toml` file:

```toml
[dependencies]
chrono = "0.4"
mime_guess = "2.0"
num_cpus = "1.0"
path-clean = "1.0"
urlencoding = "2.1"
```

BuildingClone/Download: Get the project code. If it's not already a Cargo project, create one:cargo new rusty_static_server
cd rusty_static_server
# Place the server code in src/main.rs and update Cargo.toml
Build: Navigate to the project directory in your terminal and run:# For development (faster compile, slower execution)
cargo build

# For production (slower compile, faster execution)
cargo build --release
The executable will be placed in ./target/debug/ or ./target/release/.RunningYou can run the server using Cargo or by executing the compiled binary directly.Using Cargo:cargo run -- [OPTIONS]
Example: cargo run -- -r ./public_html -p 8000Directly:# After `cargo build`
./target/debug/rusty_static_server [OPTIONS]

# After `cargo build --release`
./target/release/rusty_static_server [OPTIONS]
Example: ./target/release/rusty_static_server -r /var/www -p 80 --threads 16The server will print startup information (address, root directory, threads, log file) to the console. Press Ctrl+C to shut it down gracefully.Configuration OptionsThe server's behavior can be configured using command-line arguments:OptionAliasDescriptionDefault--host <host>-hNetwork interface address to bind to.127.0.0.1--port <port>-pPort number to listen on.8080--root <dir>-rRoot directory from which to serve files.Current working directory--threads <n>-tNumber of worker threads in the pool.Number of logical CPUs--log <file>-lPath to the log file.server.log--timeout <n>Read timeout in seconds for initial client requests.10--helpDisplay the help message and exit.Custom Error PagesTo use custom error pages:Create files named 404.html and/or 403.html.Place these files directly inside the configured --root directory.The server will automatically serve these files when the corresponding HTTP error occurs, provided they are accessible and within the root directory.

## Potential Improvements

Implement support for other HTTP methods (POST, PUT, DELETE, etc.).Add support for HTTPS/TLS encryption (e.g., using rustls or native-tls).Implement request header parsing.Add support for configuration via a file (e.g., TOML).Implement more sophisticated logging formats (e.g., Common