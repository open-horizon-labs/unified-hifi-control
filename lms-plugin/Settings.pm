package Plugins::UnifiedHiFi::Settings;

# Settings page for Unified Hi-Fi Control plugin

use strict;
use warnings;

use base qw(Slim::Web::Settings);

use Slim::Utils::Prefs;
use Slim::Utils::Log;
use Slim::Utils::Strings qw(string);

use Plugins::UnifiedHiFi::Helper;

my $log = logger('plugin.unifiedhifi');
my $prefs = preferences('plugin.unifiedhifi');

sub name {
    return 'PLUGIN_UNIFIED_HIFI';
}

sub page {
    return 'plugins/UnifiedHiFi/settings/basic.html';
}

sub prefs {
    return ($prefs, qw(autorun));
}

sub handler {
    my ($class, $client, $params, $callback, @args) = @_;

    # Handle start/stop actions
    if ($params->{'start'}) {
        Plugins::UnifiedHiFi::Helper->start();
    }
    elsif ($params->{'stop'}) {
        Plugins::UnifiedHiFi::Helper->stop();
    }

    Plugins::UnifiedHiFi::Helper->knobStatus(sub {
        my ($status) = @_;
        $params->{'knobStatus'} = $status;
        my $body = $class->SUPER::handler($client, $params);
        $callback->($client, $params, $body, @args);
    });

    return;
}

sub beforeRender {
    my ($class, $params, $client) = @_;

    # Add template variables
    $params->{'running'}    = Plugins::UnifiedHiFi::Helper->running();
    $params->{'webUrl'}     = Plugins::UnifiedHiFi::Helper->webUrl();

    # Binary download status
    $params->{'binaryStatus'}   = Plugins::UnifiedHiFi::Helper->binaryStatus();
    $params->{'binaryPlatform'} = Plugins::UnifiedHiFi::Helper->detectPlatform();

    return $class->SUPER::beforeRender($params, $client);
}

1;

__END__

=head1 NAME

Plugins::UnifiedHiFi::Settings - Web UI settings page

=head1 DESCRIPTION

Provides the settings interface for configuring the Unified Hi-Fi Control plugin.

=cut
