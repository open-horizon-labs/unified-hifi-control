package Plugins::UnifiedHiFi::Helper;

# Binary lifecycle management for Unified Hi-Fi Control
# Handles spawning, monitoring, and restarting the bridge process

use strict;
use warnings;

use File::Spec::Functions qw(catfile catdir);
use File::Path qw(make_path);
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

use constant HEALTH_CHECK_INTERVAL => 30;  # seconds
use constant MAX_RESTARTS          => 5;   # before giving up
use constant RESTART_RESET_TIME    => 300; # reset counter after 5 min stable

my ($configDir, $publicDir);

sub init {
    my ($class) = @_;

    # Use LMS's cache directory for config (contains public/ for web assets)
    $configDir = catdir($serverPrefs->get('cachedir'), 'unifiedhifi');
    unless (-d $configDir) {
        make_path($configDir) or $log->error("Failed to create data directory $configDir: $!");
    }

    $publicDir = catdir(Plugins::UnifiedHiFi::Plugin->_pluginDataFor('basedir'), 'Bin', 'public');

    # On macOS, clear quarantine flag to prevent Gatekeeper blocking unsigned binary
    if (Slim::Utils::OSDetect::OS() eq 'mac' && (my $binary = $class->bin())) {
        system('xattr', '-cr', $binary);
    }
}

# Get plugin version from install.xml
sub pluginVersion {
    return Plugins::UnifiedHiFi::Plugin->_pluginDataFor('version') || '0.0.0';
}

# Get path to the binary using LMS's built-in findBin
# Binary is in platform-specific folders: Bin/darwin/, Bin/x86_64-linux/, etc.
sub bin {
    my $class = shift;

    # Let LMS find the right binary for this platform
    # LMS knows about platform folders (darwin/, x86_64-linux/, MSWin32-x64-multi-thread/, etc.)
    my $binary = Slim::Utils::Misc::findBin('unified-hifi-control');

    if ($binary) {
        $log->debug("Found binary via LMS findbin: $binary");
        return $binary;
    }

    $log->error("No binary found for this platform. Plugin may need reinstallation.");
    return;
}

# Binary status is always 'installed' since we bundle all binaries
sub binaryStatus {
    my $class = shift;
    return $class->bin() ? 'installed' : 'missing';
}

# Start the helper process
sub start {
    my $class = shift;

    return if running();

    my $binary = $class->bin();

    unless ($binary) {
        $log->error("No suitable binary found for this platform");
        return;
    }

    return if running();

    my $port = $prefs->get('port') || 8088;

    my $lmsPort = $serverPrefs->get('httpport');

    $log->info("Starting Unified Hi-Fi Control: $binary on port $port");

    # Set environment variables for the subprocess
    # Using local ensures they're restored after Proc::Background->new() returns
    local $ENV{PORT} = $port;
    local $ENV{CONFIG_DIR} = $configDir;
    local $ENV{LMS_HOST} = '127.0.0.1';
    local $ENV{LMS_PORT} = $lmsPort;
    local $ENV{LMS_UNIFIEDHIFI_STARTED} = 'true';

    $log->debug("Running: $binary (with env: PORT=$port CONFIG_DIR=$configDir LMS_HOST=127.0.0.1 LMS_PORT=$lmsPort)");
    # Platform-specific process spawning
    if (main::ISWINDOWS) {
        # Windows: run binary directly, Proc::Background handles it
        $helperProc = Proc::Background->new(
            { 'die_upon_destroy' => 1 },
            $binary
        );
    } else {
        # Unix: use exec so shell replaces itself with binary (PID tracking works correctly)
        # This ensures $helperProc->die sends SIGTERM to the Bridge, not to a shell wrapper
        $helperProc = Proc::Background->new(
            { 'die_upon_destroy' => 1 },
            "/bin/sh", "-c", "exec '$binary' > /dev/null 2>&1"
        );
    }

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

Binaries are bundled in the plugin ZIP in LMS platform folder structure:

    Bin/
      darwin/unified-hifi-control
      x86_64-linux/unified-hifi-control
      aarch64-linux/unified-hifi-control
      arm-linux/unified-hifi-control
      MSWin32-x64-multi-thread/unified-hifi-control.exe

Web assets (CSS, images) are embedded in the binary - no separate public/
folder is needed (see ADR 002).

LMS's C<Slim::Utils::Misc::findBin()> automatically finds the correct binary
for the current platform.

=cut
