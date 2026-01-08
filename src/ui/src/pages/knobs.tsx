import { useState } from 'react';
import { useKnobs, useKnobConfig, useUpdateKnobConfig, useFirmware, useFetchFirmware } from '@/hooks';
import { PageHeader } from '@/components/layout';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Switch } from '@/components/ui/switch';
import { Skeleton } from '@/components/ui/skeleton';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Settings, Download, Battery, BatteryCharging, AlertCircle, CheckCircle2, ExternalLink } from 'lucide-react';
import type { Knob, KnobConfig } from '@/types';

function formatLastSeen(lastSeen?: string): string {
  if (!lastSeen) return 'Unknown';
  const date = new Date(lastSeen);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const diffMins = Math.floor(diffMs / 60000);

  if (diffMins < 1) return 'Just now';
  if (diffMins < 60) return `${diffMins}m ago`;
  const diffHours = Math.floor(diffMins / 60);
  if (diffHours < 24) return `${diffHours}h ago`;
  return date.toLocaleDateString();
}

interface KnobConfigDialogProps {
  knob: Knob;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

function KnobConfigDialog({ knob, open, onOpenChange }: KnobConfigDialogProps) {
  const { data: config, isLoading } = useKnobConfig(open ? knob.id : null);
  const { mutate: updateConfig, isPending } = useUpdateKnobConfig();
  const [localConfig, setLocalConfig] = useState<KnobConfig>({});
  const [message, setMessage] = useState<{ type: 'success' | 'error'; text: string } | null>(null);

  // Update local config when remote config loads
  useState(() => {
    if (config) {
      setLocalConfig(config);
    }
  });

  const handleSave = () => {
    updateConfig(
      { knobId: knob.id, config: localConfig },
      {
        onSuccess: () => {
          setMessage({ type: 'success', text: 'Configuration saved' });
          setTimeout(() => {
            setMessage(null);
            onOpenChange(false);
          }, 1500);
        },
        onError: (err) => {
          setMessage({ type: 'error', text: err.message });
        },
      }
    );
  };

  const updateField = <K extends keyof KnobConfig>(key: K, value: KnobConfig[K]) => {
    setLocalConfig((prev) => ({ ...prev, [key]: value }));
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Configure {knob.name || knob.id}</DialogTitle>
        </DialogHeader>

        {isLoading ? (
          <div className="space-y-4">
            <Skeleton className="h-10" />
            <Skeleton className="h-10" />
            <Skeleton className="h-10" />
          </div>
        ) : (
          <div className="space-y-6 py-4">
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

            <div className="space-y-2">
              <Label htmlFor="device-name">Device Name</Label>
              <Input
                id="device-name"
                value={localConfig.device_name || ''}
                onChange={(e) => updateField('device_name', e.target.value)}
                placeholder="My Knob"
              />
            </div>

            <div className="space-y-2">
              <Label htmlFor="rotation">Display Rotation</Label>
              <Select
                value={String(localConfig.display_rotation ?? 0)}
                onValueChange={(v) => updateField('display_rotation', Number(v) as 0 | 180)}
              >
                <SelectTrigger id="rotation">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="0">0° (USB on bottom)</SelectItem>
                  <SelectItem value="180">180° (USB on top)</SelectItem>
                </SelectContent>
              </Select>
            </div>

            <div className="flex items-center justify-between">
              <Label htmlFor="wifi-power-save">WiFi Power Save</Label>
              <Switch
                id="wifi-power-save"
                checked={localConfig.wifi_power_save ?? false}
                onCheckedChange={(checked) => updateField('wifi_power_save', checked)}
              />
            </div>

            <div className="space-y-2">
              <Label htmlFor="cpu-freq">CPU Frequency (MHz)</Label>
              <Select
                value={String(localConfig.cpu_freq_mhz ?? 160)}
                onValueChange={(v) => updateField('cpu_freq_mhz', Number(v))}
              >
                <SelectTrigger id="cpu-freq">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="80">80 MHz (Low power)</SelectItem>
                  <SelectItem value="160">160 MHz (Balanced)</SelectItem>
                  <SelectItem value="240">240 MHz (Performance)</SelectItem>
                </SelectContent>
              </Select>
            </div>

            <div className="space-y-2">
              <Label htmlFor="poll-interval">Poll Interval (ms)</Label>
              <Input
                id="poll-interval"
                type="number"
                value={localConfig.poll_interval_ms || 1000}
                onChange={(e) => updateField('poll_interval_ms', Number(e.target.value))}
                min={500}
                max={10000}
                step={100}
              />
            </div>
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button onClick={handleSave} disabled={isPending || isLoading}>
            {isPending ? 'Saving...' : 'Save'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function FirmwarePanel() {
  const { data: firmware, isLoading } = useFirmware();
  const { mutate: fetchFirmware, isPending } = useFetchFirmware();
  const [message, setMessage] = useState<{ type: 'success' | 'error'; text: string } | null>(null);

  const handleFetch = () => {
    fetchFirmware(undefined, {
      onSuccess: (result) => {
        setMessage({ type: 'success', text: result.message || 'Firmware fetched' });
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
        <CardTitle>Firmware</CardTitle>
        <CardDescription>
          Manage firmware for knob devices
        </CardDescription>
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

        {isLoading ? (
          <Skeleton className="h-10" />
        ) : (
          <div className="space-y-2">
            <p className="text-sm">
              <span className="text-muted-foreground">Current version:</span>{' '}
              {firmware?.current_version || 'Not available'}
            </p>
            {firmware?.available_version && firmware.available_version !== firmware.current_version && (
              <p className="text-sm text-primary">
                New version available: {firmware.available_version}
              </p>
            )}
          </div>
        )}

        <div className="flex gap-2">
          <Button onClick={handleFetch} disabled={isPending}>
            <Download className="h-4 w-4 mr-2" />
            {isPending ? 'Fetching...' : 'Fetch Latest'}
          </Button>
          <Button variant="outline" asChild>
            <a
              href="https://esp.huhn.me/"
              target="_blank"
              rel="noopener noreferrer"
            >
              <ExternalLink className="h-4 w-4 mr-2" />
              Web Flasher
            </a>
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}

export function KnobsPage() {
  const { data: knobs, isLoading } = useKnobs();
  const [selectedKnob, setSelectedKnob] = useState<Knob | null>(null);

  return (
    <div className="container mx-auto p-4 space-y-6">
      <PageHeader
        title="Knob Devices"
        description="Manage and configure your knob controllers"
      />

      <Card>
        <CardHeader>
          <CardTitle>Registered Devices</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="space-y-2">
              <Skeleton className="h-10" />
              <Skeleton className="h-10" />
              <Skeleton className="h-10" />
            </div>
          ) : !knobs?.length ? (
            <p className="text-muted-foreground text-center py-8">
              No knob devices registered yet
            </p>
          ) : (
            <div className="overflow-x-auto">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Name</TableHead>
                    <TableHead>Version</TableHead>
                    <TableHead>IP</TableHead>
                    <TableHead>Battery</TableHead>
                    <TableHead>Last Seen</TableHead>
                    <TableHead className="w-12"></TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {knobs.map((knob) => (
                    <TableRow key={knob.id}>
                      <TableCell className="font-medium">
                        {knob.name || knob.id}
                      </TableCell>
                      <TableCell className="text-muted-foreground">
                        {knob.version || '-'}
                      </TableCell>
                      <TableCell className="text-muted-foreground font-mono text-sm">
                        {knob.ip || '-'}
                      </TableCell>
                      <TableCell>
                        {knob.battery_level !== undefined ? (
                          <div className="flex items-center gap-1">
                            {knob.charging ? (
                              <BatteryCharging className="h-4 w-4 text-primary" />
                            ) : (
                              <Battery className="h-4 w-4" />
                            )}
                            <span>{knob.battery_level}%</span>
                          </div>
                        ) : (
                          '-'
                        )}
                      </TableCell>
                      <TableCell className="text-muted-foreground">
                        {formatLastSeen(knob.last_seen)}
                      </TableCell>
                      <TableCell>
                        <Button
                          variant="ghost"
                          size="icon"
                          onClick={() => setSelectedKnob(knob)}
                        >
                          <Settings className="h-4 w-4" />
                        </Button>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          )}
        </CardContent>
      </Card>

      <FirmwarePanel />

      {selectedKnob && (
        <KnobConfigDialog
          knob={selectedKnob}
          open={!!selectedKnob}
          onOpenChange={(open) => !open && setSelectedKnob(null)}
        />
      )}
    </div>
  );
}
