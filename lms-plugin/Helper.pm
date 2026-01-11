package Plugins::UnifiedHiFi::Helper;

# Binary lifecycle management for Unified Hi-Fi Control
# Handles spawning, monitoring, restarting, and on-demand downloading

use strict;
use warnings;

use File::Spec::Functions qw(catfile catdir);
use File::Path qw(make_path);
use JSON::XS;
use POSIX qw(:sys_wait_h);

use Slim::Utils::Log;
use Slim::Utils::Prefs;
use Slim::Utils::OSDetect;
use Slim::Utils::Misc;
use Slim::Utils::Timers;

my $log = logger('plugin.unifiedhifi');
my $prefs = preferences('plugin.unifiedhifi');

my $helper_pid;   # PID of helper process
my $binary;       # Path to selected binary
my $restarts = 0; # Restart counter
my $downloadInProgress = 0;  # Download state flag

use constant HEALTH_CHECK_INTERVAL => 30;  # seconds
use constant MAX_RESTARTS          => 5;   # before giving up
use constant RESTART_RESET_TIME    => 300; # reset counter after 5 min stable

# Binary download configuration
use constant BINARY_BASE_URL => 'https://github.com/open-horizon-labs/unified-hifi-control/releases/download';
use constant BINARY_MAP => {
    'darwin-arm64'   => 'unified-hifi-darwin-arm64',
    'darwin-x86_64'  => 'unified-hifi-darwin-x86_64',
    'linux-x86_64'   => 'unified-hifi-linux-x86_64',
    'linux-aarch64'  => 'unified-hifi-linux-aarch64',
    'win64'          => 'unified-hifi-win64.exe',
};

# Detect OS and return available binaries
sub binaries {
    my $class = shift;

    my $os = Slim::Utils::OSDetect::OS();
    my $details = Slim::Utils::OSDetect::details();
    my $arch = $details->{'osArch'} || $details->{'binArch'} || 'x86_64';

    my $bindir = catdir(_pluginDataFor('basedir'), 'Bin');
    my @binaries;

    if ($os eq 'win') {
        push @binaries, 'unified-hifi-win64.exe';
    }
    elsif ($os eq 'mac') {
        if ($arch =~ /arm|aarch64/i) {
            push @binaries, 'unified-hifi-darwin-arm64';
        } else {
            push @binaries, 'unified-hifi-darwin-x86_64';
        }
    }
    else {
        # Linux and other Unix-like systems
        if ($arch =~ /x86_64|amd64/i) {
            push @binaries, 'unified-hifi-linux-x86_64';
        }
        elsif ($arch =~ /aarch64|arm64/i) {
            push @binaries, 'unified-hifi-linux-aarch64';
        }
        elsif ($arch =~ /arm/i) {
            push @binaries, 'unified-hifi-linux-armv7l';
        }
        else {
            # Fallback to x86_64
            push @binaries, 'unified-hifi-linux-x86_64';
        }
    }

    # Filter to only existing files
    my @available;
    for my $bin (@binaries) {
        my $path = catfile($bindir, $bin);
        push @available, $bin if -e $path;
    }

    $log->debug("Available binaries for $os/$arch: " . join(', ', @available));
    return @available;
}

# Detect platform for binary download
sub detectPlatform {
    my $class = shift;

    my $os = Slim::Utils::OSDetect::OS();
    my $details = Slim::Utils::OSDetect::details();
    my $arch = $details->{'osArch'} || $details->{'binArch'} || 'x86_64';

    if ($os eq 'mac') {
        return $arch =~ /arm|aarch64/i ? 'darwin-arm64' : 'darwin-x86_64';
    } elsif ($os eq 'win') {
        return 'win64';
    } else {
        return $arch =~ /aarch64|arm64/i ? 'linux-aarch64' : 'linux-x86_64';
    }
}

# Get plugin version from install.xml
sub pluginVersion {
    my $class = shift;

    my $installXml = catfile(_pluginDataFor('basedir'), 'install.xml');
    if (-e $installXml) {
        open my $fh, '<', $installXml or return '0.0.0';
        my $content = do { local $/; <$fh> };
        close $fh;
        # Try element-based format first: <version>X.Y.Z</version>
        if ($content =~ /<version>([^<]+)<\/version>/) {
            return $1;
        }
        # Fallback to attribute-based: version="X.Y.Z"
        if ($content =~ /version="([^"]+)"/) {
            return $1;
        }
    }
    return '0.0.0';
}

# Check if binary needs download
sub needsBinaryDownload {
    my $class = shift;

    my $platform = $class->detectPlatform();
    my $binaryName = BINARY_MAP->{$platform};
    return 0 unless $binaryName;  # Unknown platform

    my $bindir = catdir(_pluginDataFor('basedir'), 'Bin');
    my $binaryPath = catfile($bindir, $binaryName);

    return !(-e $binaryPath && -x $binaryPath);
}

# Get binary status for UI
sub binaryStatus {
    my $class = shift;

    return 'downloading' if $downloadInProgress;
    return $class->needsBinaryDownload() ? 'not_downloaded' : 'installed';
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

    my $bindir = catdir(_pluginDataFor('basedir'), 'Bin');
    my $binaryPath = catfile($bindir, $binaryName);

    # Already exists and executable
    if (-e $binaryPath && -x $binaryPath) {
        $callback->($binaryPath) if $callback;
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
            $callback->($binaryPath) if $callback;
        } else {
            $log->error("Binary download failed: $error");
            $callback->(undef, $error) if $callback;
        }
    });

    return;  # Async - result via callback
}

# Download binary from URL
sub downloadBinary {
    my ($class, $url, $dest, $callback) = @_;

    $downloadInProgress = 1;

    # Ensure Bin directory exists
    my $bindir = catdir(_pluginDataFor('basedir'), 'Bin');
    make_path($bindir) unless -d $bindir;

    $log->info("Downloading binary from $url");

    eval {
        require Slim::Networking::SimpleAsyncHTTP;

        my $http = Slim::Networking::SimpleAsyncHTTP->new(
            sub {
                my $response = shift;
                $downloadInProgress = 0;

                if ($response->code == 200) {
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
                    $callback->(0, "HTTP " . $response->code . ": " . $response->message) if $callback;
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

# Get path to the selected binary (downloads if needed for sync start)
sub bin {
    my $class = shift;

    my $bindir = catdir(_pluginDataFor('basedir'), 'Bin');
    my @available = $class->binaries();

    # If no binaries available, check if we can use platform-specific one
    unless (@available) {
        my $platform = $class->detectPlatform();
        my $binaryName = BINARY_MAP->{$platform};
        if ($binaryName) {
            my $path = catfile($bindir, $binaryName);
            return $path if -e $path && -x $path;
        }
        return;
    }

    # Use preference or default to first available
    my $selected = $prefs->get('bin') || $available[0];

    # Validate selection
    unless (grep { $_ eq $selected } @available) {
        $selected = $available[0];
        $prefs->set('bin', $selected);
    }

    return catfile($bindir, $selected);
}

# Check if helper process is alive
sub _isAlive {
    return 0 unless $helper_pid;
    # Check if process exists and we can signal it
    my $result = kill(0, $helper_pid);
    # Also reap zombie if it died
    if (!$result) {
        waitpid($helper_pid, WNOHANG);
        $helper_pid = undef;
    }
    return $result;
}

# Start the helper process
sub start {
    my $class = shift;

    return if _isAlive();
    return if $downloadInProgress;  # Don't start while downloading

    $binary = $class->bin();

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

    return if _isAlive();

    my $port = $prefs->get('port') || 8088;
    my $loglevel = $prefs->get('loglevel') || 'info';

    # Make executable on Unix
    if (Slim::Utils::OSDetect::OS() ne 'win') {
        chmod 0755, $binaryPath;
    }

    # Build environment for subprocess
    my $configDir = Slim::Utils::OSDetect::dirsFor('prefs');
    my $lmsPort = $Slim::Web::HTTP::localPort // 9000;

    $log->info("Starting Unified Hi-Fi Control: $binaryPath on port $port");

    # Fork and exec the helper process
    my $pid = fork();

    if (!defined $pid) {
        $log->error("Failed to fork: $!");
        return;
    }

    if ($pid == 0) {
        # Child process
        # Set environment variables
        $ENV{PORT} = $port;
        $ENV{LOG_LEVEL} = $loglevel;
        $ENV{CONFIG_DIR} = $configDir;
        $ENV{LMS_HOST} = 'localhost';
        $ENV{LMS_PORT} = $lmsPort;

        # Detach from parent's file handles
        close(STDIN);
        close(STDOUT);
        close(STDERR);

        # Start new session to fully detach
        POSIX::setsid();

        # Exec the binary
        exec($binaryPath) or do {
            # If exec fails, exit child
            POSIX::_exit(1);
        };
    }

    # Parent process
    $helper_pid = $pid;
    $binary = $binaryPath;

    $log->info("Helper started with PID $helper_pid");

    # Schedule health checks
    Slim::Utils::Timers::setTimer($class, time() + HEALTH_CHECK_INTERVAL, \&_healthCheck);

    return 1;
}

# Stop the helper process
sub stop {
    my $class = shift;

    Slim::Utils::Timers::killTimers($class, \&_healthCheck);
    Slim::Utils::Timers::killTimers($class, \&_resetRestarts);

    if (_isAlive()) {
        $log->info("Stopping Unified Hi-Fi Control (PID $helper_pid)");
        kill('TERM', $helper_pid);
        # Give it a moment to terminate gracefully
        my $waited = 0;
        while (_isAlive() && $waited < 5) {
            sleep(1);
            $waited++;
        }
        # Force kill if still running
        if (_isAlive()) {
            $log->warn("Helper did not terminate gracefully, forcing kill");
            kill('KILL', $helper_pid);
        }
        waitpid($helper_pid, 0);
        $helper_pid = undef;
    }

    $restarts = 0;
}

# Check if running
sub running {
    return _isAlive();
}

# Get the web UI URL
sub webUrl {
    my $class = shift;
    my $port = $prefs->get('port') || 8088;
    return "http://localhost:$port";
}

# Health check timer callback
sub _healthCheck {
    my $class = shift;

    if ($prefs->get('autorun')) {
        if (!_isAlive()) {
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

sub _pluginDataFor {
    my $key = shift;
    my $data = Slim::Utils::PluginManager->dataForPlugin(__PACKAGE__);
    return $data ? $data->{$key} : undef;
}

# Write knob configuration to JSON file for binary to read
sub writeKnobConfig {
    my $class = shift;

    my $configDir = Slim::Utils::OSDetect::dirsFor('prefs');
    my $configFile = catfile($configDir, 'knob_config.json');

    my $config = {
        name              => $prefs->get('knob_name') || '',
        rotation_charging     => int($prefs->get('knob_rotation_charging') // 180),
        rotation_not_charging => int($prefs->get('knob_rotation_battery') // 0),
        art_mode_charging => {
            enabled     => ($prefs->get('knob_art_mode_charging') // 60) > 0,
            timeout_sec => int($prefs->get('knob_art_mode_charging') // 60),
        },
        dim_charging => {
            enabled     => ($prefs->get('knob_dim_charging') // 120) > 0,
            timeout_sec => int($prefs->get('knob_dim_charging') // 120),
        },
        sleep_charging => {
            enabled     => ($prefs->get('knob_sleep_charging') // 0) > 0,
            timeout_sec => int($prefs->get('knob_sleep_charging') // 0),
        },
        art_mode_battery => {
            enabled     => ($prefs->get('knob_art_mode_battery') // 30) > 0,
            timeout_sec => int($prefs->get('knob_art_mode_battery') // 30),
        },
        dim_battery => {
            enabled     => ($prefs->get('knob_dim_battery') // 30) > 0,
            timeout_sec => int($prefs->get('knob_dim_battery') // 30),
        },
        sleep_battery => {
            enabled     => ($prefs->get('knob_sleep_battery') // 60) > 0,
            timeout_sec => int($prefs->get('knob_sleep_battery') // 60),
        },
    };

    eval {
        open my $fh, '>', $configFile or die "Cannot write $configFile: $!";
        print $fh encode_json($config);
        close $fh;
        $log->debug("Wrote knob config to $configFile");
    };
    if ($@) {
        $log->error("Failed to write knob config: $@");
    }
}

# Get knob status from running helper (if available)
sub knobStatus {
    my $class = shift;

    return {} unless $class->running();

    my $port = $prefs->get('port') || 8088;
    my $url = "http://localhost:$port/api/knobs";

    eval {
        require LWP::UserAgent;
        my $ua = LWP::UserAgent->new(timeout => 2);
        my $response = $ua->get($url);
        if ($response->is_success) {
            my $data = decode_json($response->decoded_content);
            # Return first knob status (single knob mode)
            if ($data->{knobs} && @{$data->{knobs}}) {
                return $data->{knobs}[0];
            }
        }
    };

    return {};
}

1;

__END__

=head1 NAME

Plugins::UnifiedHiFi::Helper - Binary lifecycle management

=head1 DESCRIPTION

Manages the unified-hifi-control binary: spawning, monitoring, and restarting.

=cut
