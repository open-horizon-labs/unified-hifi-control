package Plugins::UnifiedHiFi::Plugin;

# Unified Hi-Fi Control - LMS Plugin
# Manages the unified-hifi-control bridge as a helper process

use strict;
use warnings;

use base qw(Slim::Plugin::Base);

use Slim::Utils::Prefs;
use Slim::Utils::Log;
use Slim::Utils::Strings qw(string);

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
