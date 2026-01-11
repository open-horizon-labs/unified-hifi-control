package Plugins::UnifiedHiFi::Helper;

# Binary lifecycle management for Unified Hi-Fi Control
# Handles spawning, monitoring, and restarting the helper process

use strict;
use warnings;

use File::Spec::Functions qw(catfile catdir);
use Proc::Background;
use JSON;

use Slim::Utils::Log;
use Slim::Utils::Prefs;
use Slim::Utils::OSDetect;
use Slim::Utils::Misc;
use Slim::Utils::Timers;

my $log = logger('plugin.unifiedhifi');
my $prefs = preferences('plugin.unifiedhifi');

my $helper;       # Proc::Background instance
my $binary;       # Path to selected binary
my $restarts = 0; # Restart counter

use constant HEALTH_CHECK_INTERVAL => 30;  # seconds
use constant MAX_RESTARTS          => 5;   # before giving up
use constant RESTART_RESET_TIME    => 300; # reset counter after 5 min stable

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

# Get path to the selected binary
sub bin {
    my $class = shift;

    my $bindir = catdir(_pluginDataFor('basedir'), 'Bin');
    my @available = $class->binaries();

    return unless @available;

    # Use preference or default to first available
    my $selected = $prefs->get('bin') || $available[0];

    # Validate selection
    unless (grep { $_ eq $selected } @available) {
        $selected = $available[0];
        $prefs->set('bin', $selected);
    }

    return catfile($bindir, $selected);
}

# Start the helper process
sub start {
    my $class = shift;

    return if $helper && $helper->alive;

    $binary = $class->bin();
    unless ($binary && -e $binary) {
        $log->error("No suitable binary found for this platform");
        return;
    }

    my $port = $prefs->get('port') || 8088;
    my $loglevel = $prefs->get('loglevel') || 'info';

    # Make executable on Unix
    if (Slim::Utils::OSDetect::OS() ne 'win') {
        chmod 0755, $binary;
    }

    # Build command line
    my @cmd = ($binary);

    # Build environment for subprocess (avoid polluting global %ENV)
    my $configDir = Slim::Utils::OSDetect::dirsFor('prefs');
    my $lmsPort = $Slim::Web::HTTP::localPort // 9000;

    my %childEnv = (
        %ENV,  # Inherit parent environment
        PORT       => $port,
        LOG_LEVEL  => $loglevel,
        CONFIG_DIR => $configDir,
        LMS_HOST   => 'localhost',
        LMS_PORT   => $lmsPort,
    );

    $log->info("Starting Unified Hi-Fi Control: $binary on port $port");

    eval {
        # Use local %ENV for subprocess only
        local %ENV = %childEnv;
        $helper = Proc::Background->new({'die_upon_destroy' => 1}, @cmd);
    };

    if ($@ || !$helper) {
        $log->error("Failed to start helper: $@");
        return;
    }

    # Schedule health checks
    Slim::Utils::Timers::setTimer($class, time() + HEALTH_CHECK_INTERVAL, \&_healthCheck);

    return 1;
}

# Stop the helper process
sub stop {
    my $class = shift;

    Slim::Utils::Timers::killTimers($class, \&_healthCheck);
    Slim::Utils::Timers::killTimers($class, \&_resetRestarts);

    if ($helper && $helper->alive) {
        $log->info("Stopping Unified Hi-Fi Control");
        $helper->die;
        $helper = undef;
    }

    $restarts = 0;
}

# Check if running
sub running {
    return $helper && $helper->alive;
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
        if (!$helper || !$helper->alive) {
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
    return Slim::Utils::PluginManager->dataForPlugin(__PACKAGE__)->{$key};
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
