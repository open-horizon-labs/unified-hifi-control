package Plugins::UnifiedHiFi::Plugin;

# Unified Hi-Fi Control - LMS Plugin
# Manages the unified-hifi-control bridge as a helper process

use strict;
use warnings;

use base qw(Slim::Plugin::Base);

use Slim::Utils::Prefs;
use Slim::Utils::Log;
use Slim::Utils::Strings qw(string);
use Slim::Networking::SimpleAsyncHTTP;
use HTTP::Status qw(RC_OK RC_SERVICE_UNAVAILABLE);

use Plugins::UnifiedHiFi::Helper;
use Plugins::UnifiedHiFi::Settings;

my $log = Slim::Utils::Log->addLogCategory({
    'category'     => 'plugin.unifiedhifi',
    'defaultLevel' => 'WARN',
    'description'  => 'PLUGIN_UNIFIED_HIFI',
});

my $prefs = preferences('plugin.unifiedhifi');

# Default preferences
$prefs->init({
    autorun  => 1,
    port     => 8088,
});

sub initPlugin {
    my $class = shift;

    $class->SUPER::initPlugin(@_);

    Plugins::UnifiedHiFi::Settings->new;
    Plugins::UnifiedHiFi::Helper->init;

    # The settings page polls the bridge for knob devices. It used to call the
    # bridge's own URL from the browser, which is a cross-origin request: the
    # page is served by LMS, the bridge by a different port. v4 of the bridge
    # no longer answers cross-origin browser requests unless an operator opts
    # in (UHC_ALLOWED_ORIGINS), so that call failed and the page reported the
    # bridge as not running while it was. Serve the data from LMS's own origin
    # instead: this handler fetches it server-side over loopback and relays it.
    Slim::Web::Pages->addPageFunction(
        qr{^plugins/UnifiedHiFi/knobs\.json},
        \&knobDevices,
    );

    # Start the helper if autorun is enabled
    if ($prefs->get('autorun')) {
        Plugins::UnifiedHiFi::Helper->start;
    }

    $prefs->setValidate({ 'validator' => 'intlimit', 'low' => 1024, 'high' => 65535 }, 'port');

    $log->info("Unified Hi-Fi Control plugin initialized");
}

sub shutdownPlugin {
    Plugins::UnifiedHiFi::Helper->stop;
    $log->info("Unified Hi-Fi Control plugin shutdown");
}

# GET plugins/UnifiedHiFi/knobs.json -- same-origin relay of the bridge's
# /knob/devices. Responds asynchronously: returns undef now and completes the
# HTTP response from the SimpleAsyncHTTP callback. A bridge that is down or
# not answering yields 503 with a small JSON error body, so the page's failure
# path still means "bridge not running".
sub knobDevices {
    my ($client, $params, $callback, $httpClient, $response) = @_;

    my $port = $prefs->get('port') || 8088;
    my $url  = "http://127.0.0.1:$port/knob/devices";

    Slim::Networking::SimpleAsyncHTTP->new(
        sub {
            my $http = shift;
            my $body = $http->content;
            $response->code(RC_OK);
            $response->content_type('application/json');
            $callback->($client, $params, \$body, $httpClient, $response);
        },
        sub {
            my ($http, $error) = @_;
            $log->debug("Bridge not reachable at $url: $error");
            my $body = '{"error":"bridge unreachable"}';
            $response->code(RC_SERVICE_UNAVAILABLE);
            $response->content_type('application/json');
            $callback->($client, $params, \$body, $httpClient, $response);
        },
        { timeout => 3, cache => 0 },
    )->get($url);

    return;
}

sub getDisplayName {
    return 'PLUGIN_UNIFIED_HIFI';
}

sub playerMenu { }

1;

__END__

=head1 NAME

Plugins::UnifiedHiFi::Plugin - LMS plugin for Unified Hi-Fi Control bridge

=head1 DESCRIPTION

This plugin manages the Unified Hi-Fi Control bridge as a helper process,
providing a unified control layer for Roon, LMS, HQPlayer, and hardware
control surfaces.

=head1 SEE ALSO

L<https://github.com/cloud-atlas-ai/unified-hifi-control>

=cut
