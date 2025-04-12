// Add these to your Cargo.toml dependencies:
// mime_guess = "2.0"
// chrono = "0.4"
// num_cpus = "1.0"
// path-clean = "1.0" // For robust path sanitization
// urlencoding = "2.1" // For URL decoding

use std::net::{TcpListener, TcpStream};
use std::io::{self, Read, Write, BufReader, BufRead, ErrorKind};
use std::fs::{File, OpenOptions, Metadata};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use std::sync::{Arc, Mutex};
use std::env;
use path_clean::PathClean; // Import the trait

// --- Configuration and ThreadPool structs ---

#[derive(Clone)]
struct Config {
    host: String,
    port: u16,
    root_dir: PathBuf, // Store as PathBuf for easier manipulation
    thread_pool_size: usize,
    log_file: String,
    read_timeout_secs: u64, // Added read timeout configuration
}

// Represents a thread pool for handling incoming connections.
struct ThreadPool {
    workers: Vec<Worker>,
    sender: std::sync::mpsc::Sender<Job>,
}

// A type alias for a function that represents a job to be executed.
type Job = Box<dyn FnOnce() + Send + 'static>;

// Represents a worker thread in the thread pool.
struct Worker {
    id: usize,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    // Creates a new worker thread.
    fn new(id: usize, receiver: Arc<Mutex<std::sync::mpsc::Receiver<Job>>>) -> Worker {
        let thread = thread::spawn(move || {
            loop {
                // Receive a job from the channel.
                let message = {
                    // Lock the mutex to access the receiver.
                    let guard = receiver.lock().unwrap();
                    // Wait for a message (job).
                    guard.recv()
                    // Mutex guard is dropped here, releasing the lock.
                };
                match message {
                    // If a job is received successfully, execute it.
                    Ok(job) => {
                        // println!("Worker {id} got a job; executing."); // Uncomment for verbose logging
                        job();
                    }
                    // If the channel is closed (Err received), the pool is shutting down.
                    Err(_) => {
                        // println!("Worker {id} disconnected; terminating."); // Uncomment for verbose logging
                        break; // Exit the loop to terminate the thread.
                    }
                }
            }
        });
        // Return the new worker containing its ID and thread handle.
        Worker { id, thread: Some(thread) }
    }
}

impl ThreadPool {
    // Creates a new thread pool with a specified number of worker threads.
    fn new(size: usize) -> ThreadPool {
        assert!(size > 0); // Ensure the pool size is valid.

        // Create a multi-producer, single-consumer channel for sending jobs.
        let (sender, receiver) = std::sync::mpsc::channel();
        // Wrap the receiver in an Arc<Mutex> to allow shared, thread-safe access.
        let receiver = Arc::new(Mutex::new(receiver));

        // Create a vector to hold the worker threads.
        let mut workers = Vec::with_capacity(size);

        // Spawn the specified number of worker threads.
        for id in 0..size {
            // Each worker gets a clone of the Arc pointing to the receiver mutex.
            workers.push(Worker::new(id, Arc::clone(&receiver)));
        }

        // Return the ThreadPool containing the workers and the sender.
        ThreadPool { workers, sender }
    }

    // Executes a job by sending it through the channel to an available worker.
    fn execute<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static, // Job must be Send + 'static to cross threads.
    {
        // Box the closure to create a trait object.
        let job = Box::new(f);
        // Send the job through the channel. Handle error if the receiver is disconnected.
        if self.sender.send(job).is_err() {
            eprintln!("Error sending job to worker: receiver disconnected.");
        }
    }
}

// Implement Drop for ThreadPool to ensure graceful shutdown.
impl Drop for ThreadPool {
    // Sends termination signal and waits for all threads in the pool to finish.
    fn drop(&mut self) {
        // Drop the sender explicitly. This closes the channel.
        // Workers waiting on recv() will receive an Err and terminate their loops.
        drop(self.sender.clone()); // Clone necessary as self.sender is moved otherwise

        println!("Shutting down all workers.");

        // Iterate through the workers and join their threads.
        for worker in &mut self.workers {
            // println!("Shutting down worker {}", worker.id); // Uncomment for verbose logging
            // Take ownership of the thread handle (Option allows this).
            if let Some(thread) = worker.thread.take() {
                // Wait for the thread to finish execution. Handle potential panics.
                if let Err(e) = thread.join() {
                    eprintln!("Error joining worker thread {}: {:?}", worker.id, e);
                }
            }
        }
        println!("All workers shut down.");
    }
}

// --- Connection Handling ---

// Handles an incoming TCP connection.
fn handle_connection(mut stream: TcpStream, config: Arc<Config>, log_mutex: Arc<Mutex<std::fs::File>>) {
    // Get peer address for logging.
    let peer_addr = stream.peer_addr().map_or_else(|_| "unknown".to_string(), |addr| addr.to_string());

    // *** Set read timeout for the initial request read ***
    if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(config.read_timeout_secs))) {
        log_message(&config.log_file, &format!("Failed to set read timeout for {}: {}", peer_addr, e), &log_mutex);
        // Decide if this is fatal; often logging is sufficient.
    }

    // Create a buffered reader for efficient reading.
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();

    // Read the first line of the HTTP request (the request line).
    match reader.read_line(&mut request_line) {
        Ok(0) => { // Connection closed by client before sending anything.
            log_message(&config.log_file, &format!("Client {} disconnected before sending request.", peer_addr), &log_mutex);
            return;
        }
        Ok(_) => {} // Successfully read the request line.
        Err(e) => { // Error during read.
            // Check if the error is due to the timeout.
            if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut {
                 log_message(&config.log_file, &format!("Read timeout waiting for request line from {}", peer_addr), &log_mutex);
                 // Send 408 Request Timeout response.
                 send_error_response(&mut stream, 408, "Request Timeout", &config, &log_mutex);
            } else { // Other read error.
                log_message(&config.log_file, &format!("Error reading request line from {}: {}", peer_addr, e), &log_mutex);
                // Consider sending a 500 error, but often just closing is fine.
            }
            return; // Close connection on error/timeout.
        }
    };

    // Optional: Clear or adjust timeout for subsequent reads (e.g., reading headers/body).
    // stream.set_read_timeout(None).ok();

    // Parse the request line (e.g., "GET /path HTTP/1.1").
    let request_line = request_line.trim(); // Remove trailing CRLF.
    let parts: Vec<&str> = request_line.split_whitespace().collect();

    // Validate request line format.
    if parts.len() != 3 {
        log_message(&config.log_file, &format!("Malformed request line from {}: {}", peer_addr, request_line), &log_mutex);
        send_error_response(&mut stream, 400, "Bad Request", &config, &log_mutex);
        return;
    }

    let method = parts[0];
    let path = parts[1];
    let http_version = parts[2];

    // Validate HTTP version.
    if http_version != "HTTP/1.1" {
        log_message(&config.log_file, &format!("Unsupported HTTP version from {}: {}", peer_addr, http_version), &log_mutex);
        send_error_response(&mut stream, 505, "HTTP Version Not Supported", &config, &log_mutex);
        return;
    }

    // Note: Headers are not read or parsed in this simplified version.

    log_message(&config.log_file, &format!("Received {} request for {} from {}", method, path, peer_addr), &log_mutex);

    // Dispatch based on HTTP method.
    match method {
        "GET" | "HEAD" => { // Handle GET and HEAD requests.
            // Pass `true` for GET (send body), `false` for HEAD (no body).
            handle_get_request(stream, path, method == "GET", config, log_mutex);
        }
        _ => { // Method not implemented.
            log_message(&config.log_file, &format!("Unsupported method {} from {}", method, peer_addr), &log_mutex);
            send_error_response(&mut stream, 501, "Not Implemented", &config, &log_mutex);
        }
    }
}

// Handles a GET or HEAD request.
fn handle_get_request(mut stream: TcpStream, path: &str, send_body: bool, config: Arc<Config>, log_mutex: Arc<Mutex<std::fs::File>>) {
    // *** Path Sanitization and Validation ***

    // 1. Decode URL-encoded characters (e.g., %20 -> space). Handle potential errors.
    let decoded_path = urlencoding::decode(path).map_or_else(
        |_| path.to_string(), // If decoding fails, use the original path (less safe, consider erroring)
        |p| p.into_owned()
    );

    // 2. Remove leading '/' and create a PathBuf.
    let requested_path = PathBuf::from(decoded_path.trim_start_matches('/'));

    // 3. Clean the path using path-clean (handles ., .., //).
    let cleaned_path = requested_path.clean();

    // 4. Join the cleaned path with the configured root directory.
    let mut absolute_path = config.root_dir.clone();
    absolute_path.push(cleaned_path); // Pushing PathBuf `cleaned_path`

    // 5. Canonicalize paths to resolve symlinks and get absolute paths.
    // This is crucial for the security check below.
    let canonical_root = match config.root_dir.canonicalize() {
         Ok(p) => p,
         Err(e) => {
            log_message(&config.log_file, &format!("Failed to canonicalize root dir {}: {}", config.root_dir.display(), e), &log_mutex);
            send_error_response(&mut stream, 500, "Internal Server Error", &config, &log_mutex);
            return;
         }
    };
    let canonical_absolute = match absolute_path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            // If canonicalization fails (e.g., file not found), proceed to file open check.
            // Log the error but don't send 500 yet.
            log_message(&config.log_file, &format!("Note: Could not canonicalize requested path {}: {}. Will check existence.", absolute_path.display(), e), &log_mutex);
            absolute_path // Use the non-canonicalized path for the open attempt.
        }
    };


    // 6. *** Security Check: Ensure the final path is within the root directory ***
    if !canonical_absolute.starts_with(&canonical_root) {
        log_message(&config.log_file, &format!("Forbidden path access attempt: {} resolved outside root to {}", path, canonical_absolute.display()), &log_mutex);
        send_error_response(&mut stream, 403, "Forbidden", &config, &log_mutex);
        return;
    }

    // 7. Handle directory requests: Append 'index.html'.
    // Check metadata *after* canonicalization if possible, otherwise use the path before canonicalization failure.
    let final_path = if canonical_absolute.is_dir() {
        canonical_absolute.join("index.html")
    } else {
        canonical_absolute // It's already pointing to a file (or non-existent path)
    };

    // *** File Opening and Metadata ***
    match File::open(&final_path) {
        Ok(file) => { // File opened successfully.
            match file.metadata() {
                Ok(metadata) => { // Got file metadata.
                    // Ensure we are not serving a directory (even if index.html logic failed somehow).
                    if metadata.is_dir() {
                         log_message(&config.log_file, &format!("Attempt to serve directory directly: {}", final_path.display()), &log_mutex);
                         send_error_response(&mut stream, 403, "Forbidden", &config, &log_mutex);
                         return;
                    }

                    // Determine MIME type and file size.
                    let mime_type = mime_guess::from_path(&final_path).first_or_octet_stream();
                    let file_size = metadata.len();

                    // Prepare HTTP response headers.
                    let status_line = "HTTP/1.1 200 OK";
                    let headers = format!(
                        "Content-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n", // Close connection after response for simplicity.
                        mime_type.as_ref(),
                        file_size
                    );

                    // *** Send headers first ***
                    if let Err(e) = write_headers(&mut stream, status_line, &headers) {
                         log_message(&config.log_file, &format!("Error writing headers for {}: {}", final_path.display(), e), &log_mutex);
                         return; // Stop if headers fail.
                    }

                    // *** Stream the file content if it's a GET request ***
                    if send_body { // Only send body for GET.
                        if let Err(e) = stream_file_content(file, &mut stream) {
                             // Log streaming errors (client disconnects are common).
                             log_message(&config.log_file, &format!("Error streaming file {}: {}", final_path.display(), e), &log_mutex);
                             // Connection is likely broken, nothing more we can do reliably.
                        } else {
                             // Log successful GET request completion.
                             log_message(&config.log_file, &format!("Served {} ({}, {} bytes)", final_path.display(), mime_type.as_ref(), file_size), &log_mutex);
                        }
                    } else { // HEAD request: Log completion after sending headers.
                         log_message(&config.log_file, &format!("Served HEAD for {} ({}, {} bytes)", final_path.display(), mime_type.as_ref(), file_size), &log_mutex);
                    }
                }
                Err(e) => { // Failed to get metadata for opened file.
                    log_message(&config.log_file, &format!("Error getting metadata for {}: {}", final_path.display(), e), &log_mutex);
                    send_error_response(&mut stream, 500, "Internal Server Error", &config, &log_mutex);
                }
            }
        }
        Err(e) => { // Failed to open the file.
            // Handle common errors like Not Found or Permission Denied.
            if e.kind() == ErrorKind::NotFound || e.kind() == ErrorKind::PermissionDenied {
                log_message(&config.log_file, &format!("File not found or inaccessible: {}", final_path.display()), &log_mutex);
                send_error_response(&mut stream, 404, "Not Found", &config, &log_mutex); // Try sending custom 404 page.
            } else { // Other file opening errors.
                log_message(&config.log_file, &format!("Error opening file {}: {}", final_path.display(), e), &log_mutex);
                send_error_response(&mut stream, 500, "Internal Server Error", &config, &log_mutex);
            }
        }
    };
}

// Helper function to write the status line and headers to the stream.
fn write_headers(stream: &mut TcpStream, status_line: &str, headers: &str) -> io::Result<()> {
    stream.write_all(status_line.as_bytes())?; // Write "HTTP/1.1 200 OK"
    stream.write_all(b"\r\n")?;                // CRLF after status line
    stream.write_all(headers.as_bytes())?;     // Write all header lines
    stream.write_all(b"\r\n")?;                // Empty line (CRLF) signifies end of headers.
    stream.flush()?;                           // Ensure headers are sent immediately.
    Ok(())
}

// Helper function to stream file content in chunks to the client.
fn stream_file_content(mut file: File, stream: &mut TcpStream) -> io::Result<()> {
    // Create a buffer for reading chunks (e.g., 8KB).
    let mut buffer = [0; 8192];
    loop {
        // Read a chunk from the file into the buffer.
        match file.read(&mut buffer) {
            Ok(0) => break, // End of file reached.
            Ok(n) => { // Successfully read 'n' bytes.
                // Write the read chunk to the TCP stream.
                if let Err(e) = stream.write_all(&buffer[..n]) {
                    // Handle write errors, especially BrokenPipe (client disconnected).
                    if e.kind() == ErrorKind::BrokenPipe {
                        // Log client disconnection non-critically.
                        eprintln!("Client disconnected during file streaming.");
                        return Err(e); // Return error to stop streaming.
                    } else {
                         // Log other write errors.
                         eprintln!("Error writing chunk to stream: {}", e);
                         return Err(e); // Return error.
                    }
                }
            }
            // Retry if the read operation was interrupted (e.g., by a signal).
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            // Return any other read error.
            Err(e) => return Err(e),
        }
    }
    // Ensure all buffered data is sent to the client.
    stream.flush()?;
    Ok(())
}


// Sends an error response (e.g., 404, 500) to the client.
fn send_error_response(stream: &mut TcpStream, status_code: u16, reason_phrase: &str, config: &Config, log_mutex: &Arc<Mutex<std::fs::File>>) {
    // Format the HTTP status line (e.g., "HTTP/1.1 404 Not Found").
    let status_line = format!("HTTP/1.1 {} {}", status_code, reason_phrase);

    // *** Attempt to serve a custom HTML error page (404.html or 403.html) ***
    if status_code == 404 || status_code == 403 {
        let error_page_name = format!("{}.html", status_code);
        // Construct the path relative to the configured root directory.
        let error_page_path = config.root_dir.join(error_page_name);

        // Security Check: Ensure the error page itself is within the root directory.
        // Use canonicalize to resolve potential symlinks in the error page path itself.
        if let Ok(canonical_error_path) = error_page_path.canonicalize() {
             // Must have canonical root available here too
             let canonical_root = config.root_dir.canonicalize().unwrap_or_else(|_| config.root_dir.clone());
             if canonical_error_path.starts_with(&canonical_root) {
                // Try to open the custom error file.
                if let Ok(mut file) = File::open(&canonical_error_path) {
                    // Try to get metadata to determine size.
                    if let Ok(metadata) = file.metadata(){
                        // Pre-allocate buffer for efficiency if size is known.
                        let mut contents = Vec::with_capacity(metadata.len() as usize);
                        // Read the entire error page content.
                        if file.read_to_end(&mut contents).is_ok() {
                            // Prepare headers for the custom error page.
                            let headers = format!(
                                "Content-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n",
                                contents.len()
                            );
                            // Send headers first. If successful, send the body.
                            if write_headers(stream, &status_line, &headers).is_ok() {
                                // Ignore error if client disconnected after headers were sent.
                                stream.write_all(&contents).ok();
                            }
                            return; // Custom error page sent successfully.
                        }
                    }
                }
             }
        }
    }

    // *** Fallback: Send a default plain HTML error response ***
    let body = format!("<html><head><title>{} {}</title></head><body><h1>{} {}</h1></body></html>", status_code, reason_phrase, status_code, reason_phrase);
    let headers = format!(
        "Content-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );

    // Send the default error response. Ignore errors as we're already in an error state.
    if write_headers(stream, &status_line, &headers).is_ok() {
        stream.write_all(body.as_bytes()).ok();
    }
}

// --- Logging ---
// Logs a message to the console and the configured log file.
fn log_message(log_file_path: &str, message: &str, log_mutex: &Arc<Mutex<std::fs::File>>) {
    // Get current timestamp with milliseconds.
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    // Format the log entry.
    let log_entry = format!("[{}] {}", timestamp, message);

    // Print to standard output (console).
    println!("{}", log_entry);

    // Write to the log file, protected by a mutex.
    { // Scope for the mutex guard.
        // Acquire the lock on the file handle. Panics if the mutex is poisoned.
        let mut file_guard = log_mutex.lock().unwrap();
        // Write the log entry to the file.
        if let Err(e) = writeln!(file_guard, "{}", log_entry) {
            // Print error to stderr if logging to file fails.
            eprintln!("!!! Failed to write to log file {}: {} !!!", log_file_path, e);
        }
        // Mutex guard is dropped here, releasing the lock.
    }
}


// --- Argument Parsing and Main ---

// Parses command-line arguments and returns a Config struct or an error string.
fn parse_arguments() -> Result<Config, String> {
    // Make crates available (ensure they are in Cargo.toml).
    extern crate urlencoding;
    extern crate path_clean;
    extern crate num_cpus;

    // Get command-line arguments.
    let args: Vec<String> = env::args().collect();
    // Determine default root directory (current working directory).
    let default_root = env::current_dir().map_err(|e| format!("Failed to get current directory: {}", e))?;

    // Initialize config with default values.
    let mut config = Config {
        host: "127.0.0.1".to_string(),
        port: 8080,
        root_dir: default_root, // Default root
        thread_pool_size: num_cpus::get(), // Default threads = logical cores
        log_file: "server.log".to_string(), // Default log file
        read_timeout_secs: 10, // Default read timeout: 10 seconds
    };

    // Iterate through arguments, skipping the program name (args[0]).
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--host" => {
                if i + 1 < args.len() { config.host = args[i + 1].clone(); i += 2; }
                else { return Err("--host requires an argument".to_string()); }
            }
            "-p" | "--port" => {
                if i + 1 < args.len() {
                    // Parse port number, return error on failure.
                    config.port = args[i + 1].parse().map_err(|e| format!("Invalid port '{}': {}", args[i+1], e))?;
                    i += 2;
                } else { return Err("--port requires an argument".to_string()); }
            }
            "-r" | "--root" => {
                if i + 1 < args.len() {
                    let root_path_str = &args[i + 1];
                    let root_path = PathBuf::from(root_path_str);
                     // Use canonicalize to resolve symlinks and get an absolute, verified path.
                     // This also checks if the path exists.
                    config.root_dir = std::fs::canonicalize(&root_path)
                        .map_err(|e| format!("Invalid root directory '{}': {}", root_path_str, e))?;
                    // Ensure the canonical path is actually a directory.
                    if !config.root_dir.is_dir() {
                         return Err(format!("Root path '{}' is not a directory", config.root_dir.display()));
                    }
                    i += 2;
                } else {
                    return Err("--root requires an argument".to_string());
                }
            }
            "-t" | "--threads" => {
                if i + 1 < args.len() {
                    // Parse thread count.
                    let size : usize = args[i + 1].parse().map_err(|e| format!("Invalid threads '{}': {}", args[i+1], e))?;
                    // Ensure at least one thread.
                    if size == 0 { return Err("Thread pool size must be > 0".to_string()); }
                    config.thread_pool_size = size; i += 2;
                } else { return Err("--threads requires an argument".to_string()); }
            }
             "-l" | "--log" => {
                if i + 1 < args.len() { config.log_file = args[i + 1].clone(); i += 2; }
                else { return Err("--log requires an argument".to_string()); }
            }
            "--timeout" => {
                 if i + 1 < args.len() {
                    // Parse timeout value.
                    config.read_timeout_secs = args[i + 1].parse().map_err(|e| format!("Invalid timeout '{}': {}", args[i+1], e))?;
                    i += 2;
                } else { return Err("--timeout requires an argument (seconds)".to_string()); }
            }
            "--help" => { // Display help message and exit.
                println!("Usage: server [OPTIONS]");
                println!("A simple multi-threaded HTTP server for static files.");
                println!();
                println!("Options:");
                println!("  -h, --host <host>       Set the host address (default: 127.0.0.1)");
                println!("  -p, --port <port>       Set the port number (default: 8080)");
                println!("  -r, --root <dir>        Set the root directory for serving files (default: current dir)");
                println!("  -t, --threads <size>    Set the thread pool size (default: number of logical CPUs)");
                println!("  -l, --log <file>        Set the log file path (default: server.log)");
                println!("      --timeout <secs>    Set read timeout in seconds (default: 10)");
                println!("      --help              Display this help message and exit");
                println!();
                println!("Example: ./server -r /var/www/html -p 80 --timeout 5");
                std::process::exit(0); // Exit successfully after showing help.
            }
            // Handle unknown options.
            _ => return Err(format!("Unknown option: {}", args[i])),
        }
    }
     // Final check on root directory (mostly redundant if canonicalize succeeded, but safe).
     if !config.root_dir.is_dir() {
         return Err(format!("Root path '{}' is not a directory", config.root_dir.display()));
     }
    // Return the populated Config struct.
    Ok(config)
}

// Main function: Entry point of the application.
fn main() {
    // Parse command-line arguments into a Config struct wrapped in Arc for sharing.
    let config = match parse_arguments() {
        Ok(parsed_config) => Arc::new(parsed_config),
        Err(err) => { // Handle argument parsing errors.
            eprintln!("Argument Error: {}", err);
            eprintln!("Use --help for usage information.");
            std::process::exit(1); // Exit with error code.
        }
    };

    // Open the log file (create if needed, append if exists).
    // Share the file handle using Arc<Mutex> for thread-safe logging.
    let log_file_handle = match OpenOptions::new().create(true).append(true).open(&config.log_file) {
        Ok(file) => Arc::new(Mutex::new(file)),
        Err(e) => { // Handle failure to open log file.
            eprintln!("Fatal: Could not open log file '{}': {}", config.log_file, e);
            std::process::exit(1);
        }
    };

    // Log server startup information.
    log_message(&config.log_file, &format!("Server starting with config: host={}, port={}, root={}, threads={}, log={}, timeout={}", config.host, config.port, config.root_dir.display(), config.thread_pool_size, config.log_file, config.read_timeout_secs), &log_file_handle);

    // Create the thread pool.
    let pool = ThreadPool::new(config.thread_pool_size);

    // Bind the TCP listener to the configured host and port.
    let listener = match TcpListener::bind(format!("{}:{}", config.host, config.port)) {
         Ok(l) => l,
         Err(e) => { // Handle failure to bind (e.g., port already in use).
              log_message(&config.log_file, &format!("Fatal: Could not bind to {}:{}: {}", config.host, config.port, e), &log_file_handle);
              eprintln!("Fatal: Could not bind to {}:{}: {}", config.host, config.port, e);
              std::process::exit(1);
         }
    };

    // Log successful listening start and print info to console.
    log_message(&config.log_file, &format!("Listening on {}:{}", config.host, config.port), &log_file_handle);
    println!("Server listening on http://{}:{}", config.host, config.port);
    println!("Serving files from: {}", config.root_dir.display());
    println!("Using {} worker threads.", config.thread_pool_size);
    println!("Logging to: {}", config.log_file);
    println!("Read timeout: {} seconds", config.read_timeout_secs);
    println!("Press Ctrl+C to shut down.");

    // Main server loop: Accept incoming connections.
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => { // Successfully accepted a connection.
                // Clone the Arc<Config> and Arc<Mutex<File>> to move into the thread.
                let config_clone = Arc::clone(&config);
                let log_mutex_clone = Arc::clone(&log_file_handle);
                // Dispatch the connection handling to the thread pool.
                pool.execute(|| {
                    handle_connection(stream, config_clone, log_mutex_clone);
                });
            }
            Err(e) => { // Error accepting a connection (non-fatal, usually).
                log_message(&config.log_file, &format!("Error accepting connection: {}", e), &log_file_handle);
                eprintln!("Error accepting connection: {}", e);
                // Consider adding a small delay here if accept errors happen rapidly (e.g., resource exhaustion).
            }
        }
    }

    // This part is usually only reached if the listener itself closes unexpectedly.
    // Graceful shutdown is handled by Ctrl+C -> OS signal -> process termination -> ThreadPool Drop.
    log_message(&config.log_file, "Listener closed, shutting down.", &log_file_handle);
    println!("Shutting down server.");
    // ThreadPool's Drop implementation handles worker shutdown when `pool` goes out of scope here.
}
