package Plugins::UnifiedHiFi::Helper;

# Binary lifecycle management for Unified Hi-Fi Control
# Handles spawning, monitoring, restarting, and on-demand downloading

use strict;
use warnings;

use File::Spec::Functions qw(catfile catdir);
use File::Path qw(make_path);
use File::Basename;
use JSON::XS;
use Proc::Background;

use Slim::Utils::Log;
use Slim::Utils::Prefs;
use Slim::Utils::OSDetect;
use Slim::Utils::Misc;
use Slim::Networking::SimpleAsyncHTTP;
use Slim::Utils::Timers;

my $log = logger('plugin.unifiedhifi');
my $prefs = preferences('plugin.unifiedhifi');
my $serverPrefs = preferences('server');

my $helperProc;
my $restarts = 0; # Restart counter
my $downloadInProgress = 0;  # Download state flag

use constant HEALTH_CHECK_INTERVAL => 30;  # seconds
use constant MAX_RESTARTS          => 5;   # before giving up
use constant RESTART_RESET_TIME    => 300; # reset counter after 5 min stable

# Binary download configuration
use constant BINARY_BASE_URL => 'https://github.com/open-horizon-labs/unified-hifi-control/releases/download';
use constant WEB_ASSETS_FILE => 'web-assets.tar.gz';
use constant BINARY_MAP => {
    'darwin-arm64'   => 'unified-hifi-macos-universal',
    'darwin-x86_64'  => 'unified-hifi-macos-universal',
    'linux-x86_64'   => 'unified-hifi-linux-x64',
    'linux-aarch64'  => 'unified-hifi-linux-arm64',
    'linux-armv7l'   => 'unified-hifi-linux-armv7',
    'win64'          => 'unified-hifi-win64.exe',
};

# Get the plugin data directory (survives plugin updates)
# Uses LMS server cache directory, not plugin install directory
sub dataDir {
    my $class = shift;

    my $cacheDir = $serverPrefs->get('cachedir');
    return unless $cacheDir;

    my $dataDir = catdir($cacheDir, 'UnifiedHiFi');
    make_path($dataDir) unless -d $dataDir;

    return $dataDir;
}

sub binDir {
    my $class = shift;

    my $dataDir = $class->dataDir();
    return unless $dataDir;

    my $binDir = catdir($dataDir, 'Bin');
    make_path($binDir) unless -d $binDir;

    return $binDir;
}

# Get the plugin install directory (where the plugin ZIP was extracted)
# This is where bundled binaries would be if present
sub pluginDir {
    my $class = shift;
    return Plugins::UnifiedHiFi::Plugin->_pluginDataFor('basedir');
}

# Get the Bin directory inside the plugin install location (for bundled binaries)
sub pluginBinDir {
    my $class = shift;

    my $pluginDir = $class->pluginDir();
    return unless $pluginDir;

    return catdir($pluginDir, 'Bin');
}

# Get bundled binary path (if it exists in the plugin install directory)
sub bundledBin {
    my $class = shift;

    my $pluginBinDir = $class->pluginBinDir();
    return unless $pluginBinDir;

    my $platform = $class->detectPlatform();
    my $binaryName = BINARY_MAP->{$platform};
    return unless $binaryName;

    # Bundled binaries use a generic name: unified-hifi-control (or .exe on Windows)
    my $bundledName = main::ISWINDOWS ? 'unified-hifi-control.exe' : 'unified-hifi-control';
    my $bundledPath = catfile($pluginBinDir, $bundledName);

    return (-e $bundledPath && -x _) ? $bundledPath : undef;
}

# Get bundled web assets directory (if it exists in the plugin install directory)
sub bundledPublicDir {
    my $class = shift;

    my $pluginDir = $class->pluginDir();
    return unless $pluginDir;

    my $publicDir = catdir($pluginDir, 'public');
    return (-d $publicDir) ? $publicDir : undef;
}

# Detect platform for binary download
sub detectPlatform {
    my $class = shift;

    my $details = Slim::Utils::OSDetect::details();
    my $arch = $details->{'osArch'} || $details->{'binArch'} || 'x86_64';

    if (main::ISMAC) {
        return $arch =~ /arm|aarch64/i ? 'darwin-arm64' : 'darwin-x86_64';
    } elsif (main::ISWINDOWS) {
        return 'win64';
    # Linux and other Unix-like systems
    } elsif ($arch =~ /aarch64|arm64/i) {
        return 'linux-aarch64';
    } elsif ($arch =~ /arm/i) {
        return 'linux-armv7l';
    }

    # Fallback to x86_64
    return 'linux-x86_64';
}

# Get plugin version from install.xml
sub pluginVersion {
    return Plugins::UnifiedHiFi::Plugin->_pluginDataFor('version') || '0.0.0';
}

# Check if binary needs download
# Returns false if bundled binary exists or if cached binary exists
sub needsBinaryDownload {
    my $class = shift;

    my $platform = $class->detectPlatform();
    my $binaryName = BINARY_MAP->{$platform};
    if (!$binaryName) {
        $log->error("Unsupported platform: $platform");
        return 0;
    }

    # Check for bundled binary first (no download needed)
    if ($class->bundledBin()) {
        return 0;
    }

    # Check cached binary
    my $binaryPath = $class->bin();
    return !(-e $binaryPath && -x _);
}

# Check if web assets need download
# Returns false if bundled public dir exists or if cached public dir exists
sub needsWebAssetsDownload {
    my $class = shift;

    # Check for bundled web assets first (no download needed)
    if ($class->bundledPublicDir()) {
        return 0;
    }

    # Check cached web assets
    my $publicDir = catdir($class->binDir(), 'public');
    return !(-d $publicDir);
}

# Get binary status for UI
sub binaryStatus {
    my $class = shift;

    return 'downloading' if $downloadInProgress;
    return ($class->needsBinaryDownload() || $class->needsWebAssetsDownload()) ? 'not_downloaded' : 'installed';
}

# Download binary for current platform (async-friendly)
sub ensureBinary {
    my ($class, $callback) = @_;

    my $platform = $class->detectPlatform();
    my $binaryName = BINARY_MAP->{$platform};

    unless ($binaryName) {
        $log->error("No binary available for platform: $platform");
        $callback->(undef, "Unsupported platform: $platform") if $callback;
        return;
    }

    my $binaryPath = $class->bin();

    # Callback wrapper that ensures web assets before final callback
    my $withWebAssets = sub {
        my $path = shift;
        $class->ensureWebAssets(sub {
            my ($success, $error) = @_;
            if ($success) {
                $callback->($path) if $callback;
            } else {
                $callback->(undef, "Web assets download failed: $error") if $callback;
            }
        });
    };

    # Already exists and executable
    if (-e $binaryPath && -x _) {
        # Binary exists, ensure web assets too
        $withWebAssets->($binaryPath);
        return $binaryPath;
    }

    # Need to download
    $log->info("Binary not found, downloading $binaryName for $platform...");

    my $version = $class->pluginVersion();
    my $url = BINARY_BASE_URL . "/v$version/$binaryName";

    $class->downloadBinary($url, $binaryPath, sub {
        my ($success, $error) = @_;
        if ($success) {
            chmod 0755, $binaryPath;
            $log->info("Binary downloaded successfully: $binaryPath");
            # Now ensure web assets
            $withWebAssets->($binaryPath);
        } else {
            $log->error("Binary download failed: $error");
            $callback->(undef, $error) if $callback;
        }
    });

    return;  # Async - result via callback
}

# Download and extract web assets tarball
sub ensureWebAssets {
    my ($class, $callback) = @_;

    # Check for bundled web assets first
    my $bundledPublic = $class->bundledPublicDir();
    if ($bundledPublic) {
        $log->debug("Using bundled web assets at $bundledPublic");
        $callback->(1) if $callback;
        return 1;
    }

    my $binDir = $class->binDir();
    my $publicDir = catdir($binDir, 'public');

    # Already exists in cache
    if (-d $publicDir) {
        $log->debug("Web assets already present at $publicDir");
        $callback->(1) if $callback;
        return 1;
    }

    $log->info("Web assets not found, downloading...");

    my $version = $class->pluginVersion();
    my $url = BINARY_BASE_URL . "/v$version/" . WEB_ASSETS_FILE;
    my $tarballPath = catfile($binDir, WEB_ASSETS_FILE);

    $class->downloadFile($url, $tarballPath, sub {
        my ($success, $error) = @_;
        if ($success) {
            # Extract tarball
            my $result = $class->extractTarball($tarballPath, $binDir);
            unlink $tarballPath;  # Clean up tarball
            if ($result) {
                $log->info("Web assets extracted to $publicDir");
                $callback->(1) if $callback;
            } else {
                $callback->(0, "Failed to extract web assets") if $callback;
            }
        } else {
            $log->error("Web assets download failed: $error");
            $callback->(0, $error) if $callback;
        }
    });

    return;  # Async
}

# Extract tarball to destination directory
sub extractTarball {
    my ($class, $tarball, $destDir) = @_;

    eval {
        require Archive::Tar;
        my $tar = Archive::Tar->new($tarball);
        $tar->setcwd($destDir);
        $tar->extract();
        return 1;
    };

    if ($@) {
        # Fallback to system tar command
        $log->debug("Archive::Tar not available, using system tar");
        my $result = system("tar", "-xzf", $tarball, "-C", $destDir);
        return $result == 0;
    }

    return 1;
}

# Download file from URL (with redirect handling) - generic version
sub downloadFile {
    my ($class, $url, $dest, $callback, $redirectCount) = @_;
    $redirectCount //= 0;

    # Prevent infinite redirects
    if ($redirectCount > 5) {
        $downloadInProgress = 0;
        $callback->(0, "Too many redirects") if $callback;
        return;
    }

    $downloadInProgress = 1 if $redirectCount == 0;

    # Ensure destination directory exists
    my $destDir = dirname($dest);
    make_path($destDir) unless -d $destDir;

    $log->info("Downloading from $url" . ($redirectCount ? " (redirect $redirectCount)" : ""));

    eval {
        my $http = Slim::Networking::SimpleAsyncHTTP->new(
            sub {
                my $response = shift;

                my $code = $response->code;

                # Handle redirects (301, 302, 303, 307, 308)
                if ($code >= 300 && $code < 400) {
                    my $location = $response->headers->header('Location');
                    if ($location) {
                        $log->debug("Following redirect to: $location");
                        $class->downloadFile($location, $dest, $callback, $redirectCount + 1);
                        return;
                    }
                }

                $downloadInProgress = 0;

                if ($code == 200) {
                    # Write to file
                    open my $fh, '>', $dest or do {
                        $callback->(0, "Cannot write to $dest: $!") if $callback;
                        return;
                    };
                    binmode $fh;
                    print $fh $response->content;
                    close $fh;

                    $callback->(1) if $callback;
                } else {
                    $callback->(0, "HTTP $code: " . $response->message) if $callback;
                }
            },
            sub {
                my ($response, $error) = @_;
                $downloadInProgress = 0;
                $callback->(0, $error // "Download failed") if $callback;
            },
            {
                timeout => 300,  # 5 minute timeout
            }
        );

        $http->get($url);
    };

    if ($@) {
        $downloadInProgress = 0;
        $log->error("Download error: $@");
        $callback->(0, $@) if $callback;
    }
}

# Download binary from URL (with redirect handling)
sub downloadBinary {
    my ($class, $url, $dest, $callback, $redirectCount) = @_;
    $redirectCount //= 0;

    # Prevent infinite redirects
    if ($redirectCount > 5) {
        $downloadInProgress = 0;
        $callback->(0, "Too many redirects") if $callback;
        return;
    }

    $downloadInProgress = 1 if $redirectCount == 0;

    # Ensure Bin directory exists
    my $bindir = $class->binDir();
    make_path($bindir) unless -d $bindir;

    $log->info("Downloading binary from $url" . ($redirectCount ? " (redirect $redirectCount)" : ""));

    eval {
        my $http = Slim::Networking::SimpleAsyncHTTP->new(
            sub {
                my $response = shift;

                my $code = $response->code;

                # Handle redirects (301, 302, 303, 307, 308)
                if ($code >= 300 && $code < 400) {
                    my $location = $response->headers->header('Location');
                    if ($location) {
                        $log->debug("Following redirect to: $location");
                        $class->downloadBinary($location, $dest, $callback, $redirectCount + 1);
                        return;
                    }
                }

                $downloadInProgress = 0;

                if ($code == 200) {
                    # Write binary to file
                    open my $fh, '>', $dest or do {
                        $callback->(0, "Cannot write to $dest: $!") if $callback;
                        return;
                    };
                    binmode $fh;
                    print $fh $response->content;
                    close $fh;

                    $callback->(1) if $callback;
                } else {
                    $callback->(0, "HTTP $code: " . $response->message) if $callback;
                }
            },
            sub {
                my ($response, $error) = @_;
                $downloadInProgress = 0;
                $callback->(0, $error // "Download failed") if $callback;
            },
            {
                timeout => 300,  # 5 minute timeout for large binary
            }
        );

        $http->get($url);
    };

    if ($@) {
        $downloadInProgress = 0;
        $log->error("Download error: $@");
        $callback->(0, $@) if $callback;
    }
}

# Get path to the binary to use
# Priority: 1. Bundled binary (in plugin install dir), 2. Cached binary (in LMS cache)
sub bin {
    my $class = shift;

    # Check for bundled binary first
    my $bundled = $class->bundledBin();
    if ($bundled) {
        $log->debug("Using bundled binary: $bundled");
        return $bundled;
    }

    # Fall back to cached binary
    my $selected = $prefs->get('bin');

    if (!$selected) {
        $selected = BINARY_MAP->{$class->detectPlatform()};
        $prefs->set('bin', $selected) if $selected;
    }

    return unless $selected;

    my $binaryPath = catfile($class->binDir(), $selected);
    chmod 0755, $binaryPath if !main::ISWINDOWS && -f $binaryPath && !-x _;

    return $binaryPath;
}

# Start the helper process
sub start {
    my $class = shift;

    return if running();
    return if $downloadInProgress;  # Don't start while downloading

    my $binary = $class->bin();

    # If no binary, try to download it
    unless ($binary && -e $binary) {
        if ($class->needsBinaryDownload()) {
            $log->info("Binary not found, initiating download...");
            $class->ensureBinary(sub {
                my ($path, $error) = @_;
                if ($path) {
                    # Download complete, now start
                    $class->_doStart($path);
                } else {
                    $log->error("Cannot start: $error");
                }
            });
            return;  # Will start via callback
        }
        $log->error("No suitable binary found for this platform");
        return;
    }

    $class->_doStart($binary);
}

# Internal: actually start the helper process
sub _doStart {
    my ($class, $binaryPath) = @_;

    return if running();

    my $port = $prefs->get('port') || 8088;
    my $loglevel = $prefs->get('loglevel') || 'info';

    # Build environment for subprocess
    # Use plugin's data directory (survives plugin updates)
    my $configDir = $class->dataDir();
    my $lmsPort = $serverPrefs->get('httpport');

    $log->info("Starting Unified Hi-Fi Control: $binaryPath on port $port");

    # On macOS, clear quarantine flag to prevent Gatekeeper blocking unsigned binary
    if (Slim::Utils::OSDetect::OS() eq 'mac') {
        system('xattr', '-cr', $binaryPath);
    }

    # Set environment variables for the subprocess
    # Using local ensures they're restored after Proc::Background->new() returns
    local $ENV{PORT} = $port;
    local $ENV{LOG_LEVEL} = $loglevel;
    local $ENV{CONFIG_DIR} = $configDir;
    local $ENV{LMS_HOST} = '127.0.0.1';
    local $ENV{LMS_PORT} = $lmsPort;
    local $ENV{LMS_UNIFIEDHIFI_STARTED} = 'true';

    $log->debug("Running: $binaryPath (with env: PORT=$port LOG_LEVEL=$loglevel CONFIG_DIR=$configDir LMS_HOST=127.0.0.1 LMS_PORT=$lmsPort)");

    # Handle logging based on prefs
    my $logDest = '/dev/null';
    if ($prefs->get('logging')) {
        my $logFile = catfile(Slim::Utils::OSDetect::dirsFor('log'), 'unifiedhifi.log');

        # Erase log on restart if enabled (prevents unbounded growth)
        if ($prefs->get('eraselog')) {
            unlink $logFile;
        }

        $logDest = $logFile;
        $log->info("Bridge logging to: $logFile");
    }

    # Run via exec so shell replaces itself with binary (PID tracking works correctly)
    # This ensures $helperProc->die sends SIGTERM to the Bridge, not to a shell wrapper
    $helperProc = Proc::Background->new(
        { 'die_upon_destroy' => 1 },
        "/bin/sh", "-c", "exec '$binaryPath' >> '$logDest' 2>&1"
    );

    # Schedule health checks
    Slim::Utils::Timers::setTimer($class, time() + HEALTH_CHECK_INTERVAL, \&_healthCheck);

    return 1;
}

# Stop the helper process (non-blocking to avoid freezing LMS shutdown)
sub stop {
    my $class = shift;

    Slim::Utils::Timers::killTimers($class, \&_healthCheck);
    Slim::Utils::Timers::killTimers($class, \&_resetRestarts);

    $helperProc && $helperProc->die;
    $helperProc && $helperProc->wait;  # Reap zombie process
    $restarts = 0;
}

# Check if helper process is alive
sub running {
    return $helperProc && $helperProc->alive;
}

# Get the web UI URL
sub webUrl {
    my $class = shift;
    my $port = $prefs->get('port') || 8088;
    return sprintf('http://%s:%d', Slim::Utils::Network::serverAddr(), $port);
}

# Health check timer callback
sub _healthCheck {
    my $class = shift;

    if ($prefs->get('autorun')) {
        if (!running()) {
            $log->warn("Helper process died unexpectedly");

            if ($restarts < MAX_RESTARTS) {
                $restarts++;
                $log->info("Restarting helper (attempt $restarts/" . MAX_RESTARTS . ")");
                $class->start();
            } else {
                $log->error("Max restarts exceeded, auto-restart disabled until manual intervention");
                # Continue health checks but don't auto-restart
                # User can manually start via settings, which resets $restarts
            }
        } else {
            $log->debug("Helper running with PID " . $helperProc->pid);

            # Process is healthy, schedule restart counter reset
            if ($restarts > 0) {
                Slim::Utils::Timers::killTimers($class, \&_resetRestarts);
                Slim::Utils::Timers::setTimer(
                    $class,
                    time() + RESTART_RESET_TIME,
                    \&_resetRestarts
                );
            }
        }

        # Always schedule next health check (even after max restarts)
        # This allows monitoring to resume if user manually restarts
        Slim::Utils::Timers::setTimer(
            $class,
            time() + HEALTH_CHECK_INTERVAL,
            \&_healthCheck
        );
    }
}

sub _resetRestarts {
    $restarts = 0;
}

# Get knob status from running helper (if available)
sub knobStatus {
    my ($class, $cb) = @_;
    _helperAPICall('knob/devices', sub {
        my ($data) = @_;

        # Return first knob status (single knob mode)
        if ($data->{knobs} && @{$data->{knobs}}) {
            $cb->($data);
            return;
        }
        $cb->({});
    });
}

sub _helperAPICall {
    my ($endpoint, $cb) = @_;

    return $cb->({}) unless __PACKAGE__->running();

    my $port = $prefs->get('port') || 8088;
    my $url = "http://localhost:$port/$endpoint";

    main::DEBUGLOG && $log->is_debug && $log->debug("Calling bridge: $url");

    Slim::Networking::SimpleAsyncHTTP->new(
        sub {
            my $response = shift;

            if ($response->code == 200) {
                my $data = eval { decode_json($response->content) };
                $log->error("JSON decode error: $@ " . $response->content) if $@;
                main::DEBUGLOG && $log->is_debug && $log->debug("Received response from bridge: " . encode_json($data)) unless $@;
                return $cb->($data) if $data;
            }

            $log->warn("Unexpected response from bridge: " . $response->code);
            $cb->({});
        },
        sub {
            my ($response, $error) = @_;
            $log->error($error);
            $cb->({ error => $error });
        },
        { timeout => 2 }
    )->get($url);
}

1;

__END__

=head1 NAME

Plugins::UnifiedHiFi::Helper - Binary lifecycle management

=head1 DESCRIPTION

Manages the unified-hifi-control binary: spawning, monitoring, and restarting.

Supports two modes of binary deployment:

=over 4

=item Bundled (full ZIP)

The plugin ZIP includes pre-built binaries in C<Bin/> and web assets in C<public/>.
These are used directly without any download. Used for PR testing and offline installs.

=item Bootstrap (default)

The plugin ZIP includes only Perl code. On first run, the binary and web assets
are downloaded from GitHub releases and cached in the LMS cache directory.
This is the default for release builds - smaller download, works on any platform.

=back

Binary lookup priority: bundled (plugin install dir) > cached (LMS cache dir).

=cut
