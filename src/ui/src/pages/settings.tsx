import { useState } from 'react';
import {
  useSettings,
  useUpdateSettings,
  useHqpStatus,
  useConfigureHqp,
  useLmsStatus,
  useConfigureLms,
  useStatus,
} from '@/hooks';
import { PageHeader } from '@/components/layout';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Skeleton } from '@/components/ui/skeleton';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Separator } from '@/components/ui/separator';
import { AlertCircle, CheckCircle2, ChevronDown, Circle } from 'lucide-react';
import type { HqpConfig, LmsConfig } from '@/types';

function ConnectionIndicator({ connected }: { connected: boolean }) {
  return (
    <div className="flex items-center gap-2">
      <Circle
        className={`h-3 w-3 ${connected ? 'fill-green-500 text-green-500' : 'fill-red-500 text-red-500'}`}
      />
      <span className="text-sm text-muted-foreground">
        {connected ? 'Connected' : 'Disconnected'}
      </span>
    </div>
  );
}

function HqplayerConfig() {
  const { data: status, isLoading: statusLoading } = useHqpStatus();
  const { mutate: configure, isPending } = useConfigureHqp();
  const [config, setConfig] = useState<HqpConfig>({ host: '', port: 4321 });
  const [message, setMessage] = useState<{ type: 'success' | 'error'; text: string } | null>(null);

  const handleSave = () => {
    configure(config, {
      onSuccess: () => {
        setMessage({ type: 'success', text: 'HQPlayer configuration saved' });
        setTimeout(() => setMessage(null), 3000);
      },
      onError: (err) => {
        setMessage({ type: 'error', text: err.message });
      },
    });
  };

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between">
          <div>
            <CardTitle>HQPlayer</CardTitle>
            <CardDescription>Configure HQPlayer connection</CardDescription>
          </div>
          {statusLoading ? (
            <Skeleton className="h-5 w-24" />
          ) : (
            <ConnectionIndicator connected={status?.connected ?? false} />
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        {message && (
          <Alert variant={message.type === 'error' ? 'destructive' : 'default'}>
            {message.type === 'success' ? (
              <CheckCircle2 className="h-4 w-4" />
            ) : (
              <AlertCircle className="h-4 w-4" />
            )}
            <AlertDescription>{message.text}</AlertDescription>
          </Alert>
        )}

        <div className="grid gap-4 sm:grid-cols-2">
          <div className="space-y-2">
            <Label htmlFor="hqp-host">Host</Label>
            <Input
              id="hqp-host"
              value={config.host}
              onChange={(e) => setConfig({ ...config, host: e.target.value })}
              placeholder="192.168.1.100"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="hqp-port">Port</Label>
            <Input
              id="hqp-port"
              type="number"
              value={config.port}
              onChange={(e) => setConfig({ ...config, port: Number(e.target.value) })}
            />
          </div>
        </div>

        <Separator />
        <p className="text-sm text-muted-foreground">
          For HQPlayer Embedded, enter web UI credentials:
        </p>

        <div className="grid gap-4 sm:grid-cols-2">
          <div className="space-y-2">
            <Label htmlFor="hqp-username">Username (optional)</Label>
            <Input
              id="hqp-username"
              value={config.web_username || ''}
              onChange={(e) => setConfig({ ...config, web_username: e.target.value })}
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="hqp-password">Password (optional)</Label>
            <Input
              id="hqp-password"
              type="password"
              value={config.web_password || ''}
              onChange={(e) => setConfig({ ...config, web_password: e.target.value })}
            />
          </div>
        </div>

        <Button onClick={handleSave} disabled={isPending}>
          {isPending ? 'Saving...' : 'Save Configuration'}
        </Button>
      </CardContent>
    </Card>
  );
}

function LyrionConfig() {
  const { data: status, isLoading: statusLoading } = useLmsStatus();
  const { mutate: configure, isPending } = useConfigureLms();
  const [config, setConfig] = useState<LmsConfig>({ host: '', port: 9000 });
  const [message, setMessage] = useState<{ type: 'success' | 'error'; text: string } | null>(null);

  const handleSave = () => {
    configure(config, {
      onSuccess: () => {
        setMessage({ type: 'success', text: 'Lyrion configuration saved' });
        setTimeout(() => setMessage(null), 3000);
      },
      onError: (err) => {
        setMessage({ type: 'error', text: err.message });
      },
    });
  };

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between">
          <div>
            <CardTitle>Lyrion Music Server</CardTitle>
            <CardDescription>
              Configure Lyrion (formerly Logitech Media Server) connection
            </CardDescription>
          </div>
          {statusLoading ? (
            <Skeleton className="h-5 w-24" />
          ) : (
            <div className="text-right">
              <ConnectionIndicator connected={status?.connected ?? false} />
              {status?.player_count !== undefined && (
                <p className="text-xs text-muted-foreground mt-1">
                  {status.player_count} player{status.player_count !== 1 ? 's' : ''}
                </p>
              )}
            </div>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        {message && (
          <Alert variant={message.type === 'error' ? 'destructive' : 'default'}>
            {message.type === 'success' ? (
              <CheckCircle2 className="h-4 w-4" />
            ) : (
              <AlertCircle className="h-4 w-4" />
            )}
            <AlertDescription>{message.text}</AlertDescription>
          </Alert>
        )}

        <div className="grid gap-4 sm:grid-cols-2">
          <div className="space-y-2">
            <Label htmlFor="lms-host">Host</Label>
            <Input
              id="lms-host"
              value={config.host}
              onChange={(e) => setConfig({ ...config, host: e.target.value })}
              placeholder="192.168.1.100"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="lms-port">Port</Label>
            <Input
              id="lms-port"
              type="number"
              value={config.port}
              onChange={(e) => setConfig({ ...config, port: Number(e.target.value) })}
            />
          </div>
        </div>

        <div className="grid gap-4 sm:grid-cols-2">
          <div className="space-y-2">
            <Label htmlFor="lms-username">Username (optional)</Label>
            <Input
              id="lms-username"
              value={config.username || ''}
              onChange={(e) => setConfig({ ...config, username: e.target.value })}
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="lms-password">Password (optional)</Label>
            <Input
              id="lms-password"
              type="password"
              value={config.password || ''}
              onChange={(e) => setConfig({ ...config, password: e.target.value })}
            />
          </div>
        </div>

        <Button onClick={handleSave} disabled={isPending}>
          {isPending ? 'Saving...' : 'Save Configuration'}
        </Button>
      </CardContent>
    </Card>
  );
}

function BackendToggles() {
  const { data: settings, isLoading } = useSettings();
  const { mutate: updateSettings, isPending } = useUpdateSettings();
  const [message, setMessage] = useState<{ type: 'success' | 'error'; text: string } | null>(null);

  const handleToggle = (backend: string, enabled: boolean) => {
    updateSettings(
      {
        ...settings,
        backends: {
          ...settings?.backends,
          [backend]: enabled,
        },
      },
      {
        onSuccess: () => {
          setMessage({ type: 'success', text: `${backend} ${enabled ? 'enabled' : 'disabled'}` });
          setTimeout(() => setMessage(null), 2000);
        },
        onError: (err) => {
          setMessage({ type: 'error', text: err.message });
        },
      }
    );
  };

  const handleHideKnobs = (hide: boolean) => {
    updateSettings(
      { ...settings, hideKnobsPage: hide },
      {
        onSuccess: () => {
          setMessage({ type: 'success', text: 'Setting saved' });
          setTimeout(() => setMessage(null), 2000);
        },
        onError: (err) => {
          setMessage({ type: 'error', text: err.message });
        },
      }
    );
  };

  if (isLoading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Audio Backends</CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-40" />
        </CardContent>
      </Card>
    );
  }

  const backends = [
    { key: 'roon', label: 'Roon' },
    { key: 'upnp', label: 'UPnP/DLNA' },
    { key: 'openhome', label: 'OpenHome' },
    { key: 'lyrion', label: 'Lyrion' },
  ];

  return (
    <Card>
      <CardHeader>
        <CardTitle>Settings</CardTitle>
        <CardDescription>Configure audio backends and UI preferences</CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        {message && (
          <Alert variant={message.type === 'error' ? 'destructive' : 'default'}>
            {message.type === 'success' ? (
              <CheckCircle2 className="h-4 w-4" />
            ) : (
              <AlertCircle className="h-4 w-4" />
            )}
            <AlertDescription>{message.text}</AlertDescription>
          </Alert>
        )}

        <div className="space-y-4">
          <h3 className="text-sm font-medium">Audio Backends</h3>
          {backends.map(({ key, label }) => (
            <div key={key} className="flex items-center justify-between">
              <Label htmlFor={`backend-${key}`}>{label}</Label>
              <Switch
                id={`backend-${key}`}
                checked={settings?.backends?.[key as keyof typeof settings.backends] ?? true}
                onCheckedChange={(checked) => handleToggle(key, checked)}
                disabled={isPending}
              />
            </div>
          ))}
        </div>

        <Separator />

        <div className="space-y-4">
          <h3 className="text-sm font-medium">UI Preferences</h3>
          <div className="flex items-center justify-between">
            <Label htmlFor="hide-knobs">Hide Knobs Page</Label>
            <Switch
              id="hide-knobs"
              checked={settings?.hideKnobsPage ?? false}
              onCheckedChange={handleHideKnobs}
              disabled={isPending}
            />
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

function StatusPanel() {
  const { data: status, isLoading } = useStatus();
  const [isOpen, setIsOpen] = useState(false);

  return (
    <Card>
      <CardHeader>
        <CardTitle>System Status</CardTitle>
        <CardDescription>View current system state and debug information</CardDescription>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <Skeleton className="h-40" />
        ) : (
          <Collapsible open={isOpen} onOpenChange={setIsOpen}>
            <CollapsibleTrigger asChild>
              <Button variant="outline" className="w-full justify-between">
                <span>View Raw Status</span>
                <ChevronDown
                  className={`h-4 w-4 transition-transform ${isOpen ? 'rotate-180' : ''}`}
                />
              </Button>
            </CollapsibleTrigger>
            <CollapsibleContent>
              <pre className="mt-4 p-4 bg-muted rounded-md text-xs overflow-auto max-h-96">
                {JSON.stringify(status, null, 2)}
              </pre>
            </CollapsibleContent>
          </Collapsible>
        )}
      </CardContent>
    </Card>
  );
}

export function SettingsPage() {
  return (
    <div className="container mx-auto p-4 space-y-6">
      <PageHeader
        title="Settings"
        description="Configure your audio system"
      />

      <Tabs defaultValue="backends" className="space-y-6">
        <TabsList>
          <TabsTrigger value="backends">Backends</TabsTrigger>
          <TabsTrigger value="hqplayer">HQPlayer</TabsTrigger>
          <TabsTrigger value="lyrion">Lyrion</TabsTrigger>
          <TabsTrigger value="status">Status</TabsTrigger>
        </TabsList>

        <TabsContent value="backends">
          <BackendToggles />
        </TabsContent>

        <TabsContent value="hqplayer">
          <HqplayerConfig />
        </TabsContent>

        <TabsContent value="lyrion">
          <LyrionConfig />
        </TabsContent>

        <TabsContent value="status">
          <StatusPanel />
        </TabsContent>
      </Tabs>
    </div>
  );
}
