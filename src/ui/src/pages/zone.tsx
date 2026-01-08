import { useState, useEffect } from 'react';
import { useStatus, useHqpStatus, useHqpProfiles, useLoadHqpProfile, useHqpPipeline, useUpdateHqpPipeline } from '@/hooks';
import { PageHeader } from '@/components/layout';
import { ZoneCard, ZoneSelector } from '@/components/zones';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { AlertCircle, CheckCircle2 } from 'lucide-react';
import type { Zone } from '@/types';

const STORAGE_KEY = 'selected-zone-id';

function DspPipeline() {
  const { data: pipeline, isLoading } = useHqpPipeline();
  const { mutate: updatePipeline, isPending } = useUpdateHqpPipeline();
  const [message, setMessage] = useState<{ type: 'success' | 'error'; text: string } | null>(null);

  if (isLoading || !pipeline) {
    return <Skeleton className="h-48" />;
  }

  const { settings, options } = pipeline;

  const handleChange = (key: string, value: string) => {
    updatePipeline(
      { ...settings, [key]: value },
      {
        onSuccess: () => {
          setMessage({ type: 'success', text: 'Pipeline updated' });
          setTimeout(() => setMessage(null), 2000);
        },
        onError: (err) => {
          setMessage({ type: 'error', text: err.message });
        },
      }
    );
  };

  const renderSelect = (label: string, key: string, optionsKey: string) => {
    const items = options[optionsKey as keyof typeof options] as string[] | undefined;
    if (!items?.length) return null;

    return (
      <div className="space-y-2">
        <label className="text-sm font-medium">{label}</label>
        <Select
          value={settings[key as keyof typeof settings] || ''}
          onValueChange={(v) => handleChange(key, v)}
          disabled={isPending}
        >
          <SelectTrigger>
            <SelectValue placeholder={`Select ${label.toLowerCase()}`} />
          </SelectTrigger>
          <SelectContent>
            {items.map((item) => (
              <SelectItem key={item} value={item}>
                {item}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
    );
  };

  return (
    <div className="space-y-4">
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

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {renderSelect('Mode', 'mode', 'modes')}
        {renderSelect('Sample Rate', 'rate', 'rates')}
        {renderSelect('Filter (1x)', 'filter_1x', 'filters_1x')}
        {renderSelect('Filter (Nx)', 'filter_nx', 'filters_nx')}
        {renderSelect('Shaper', 'shaper', 'shapers')}
        {renderSelect('Dither', 'dither', 'dithers')}
        {renderSelect('Modulator', 'modulator', 'modulators')}
      </div>
    </div>
  );
}

function ProfileSelector() {
  const { data: profiles, isLoading } = useHqpProfiles();
  const { mutate: loadProfile, isPending } = useLoadHqpProfile();
  const [selectedProfile, setSelectedProfile] = useState<string>('');
  const [message, setMessage] = useState<{ type: 'success' | 'error'; text: string } | null>(null);

  if (isLoading || !profiles?.length) {
    return null;
  }

  const handleLoad = () => {
    if (!selectedProfile) return;
    loadProfile(selectedProfile, {
      onSuccess: (result) => {
        setMessage({ type: 'success', text: result.message || 'Profile loaded' });
        setTimeout(() => setMessage(null), 3000);
      },
      onError: (err) => {
        setMessage({ type: 'error', text: err.message });
      },
    });
  };

  return (
    <div className="space-y-4">
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

      <div className="flex gap-2">
        <Select value={selectedProfile} onValueChange={setSelectedProfile}>
          <SelectTrigger className="flex-1">
            <SelectValue placeholder="Select a profile" />
          </SelectTrigger>
          <SelectContent>
            {profiles.map((profile) => (
              <SelectItem key={profile.name} value={profile.name}>
                {profile.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button
          onClick={handleLoad}
          disabled={!selectedProfile || isPending}
        >
          Load
        </Button>
      </div>
    </div>
  );
}

export function ZonePage() {
  const [selectedZoneId, setSelectedZoneId] = useState<string | null>(() => {
    return localStorage.getItem(STORAGE_KEY);
  });

  const { data: status, isLoading } = useStatus();
  const { data: hqpStatus } = useHqpStatus();

  const zones = status?.zones ? Object.values(status.zones) : [];
  const selectedZone = zones.find((z: Zone) => z.zone_id === selectedZoneId);

  // Persist selection
  useEffect(() => {
    if (selectedZoneId) {
      localStorage.setItem(STORAGE_KEY, selectedZoneId);
    } else {
      localStorage.removeItem(STORAGE_KEY);
    }
  }, [selectedZoneId]);

  // Auto-select first zone if none selected
  useEffect(() => {
    if (!selectedZoneId && zones.length > 0) {
      setSelectedZoneId(zones[0].zone_id);
    }
  }, [selectedZoneId, zones]);

  if (isLoading) {
    return (
      <div className="container mx-auto p-4 space-y-6">
        <PageHeader title="Zone Focus" />
        <Skeleton className="h-10 w-64" />
        <Skeleton className="h-48" />
      </div>
    );
  }

  return (
    <div className="container mx-auto p-4 space-y-6">
      <PageHeader title="Zone Focus" />

      <ZoneSelector
        zones={zones}
        selectedZoneId={selectedZoneId}
        onSelect={setSelectedZoneId}
      />

      {selectedZone && (
        <ZoneCard zone={selectedZone} size="lg" />
      )}

      {hqpStatus?.connected && (
        <>
          <Card>
            <CardHeader>
              <CardTitle>HQPlayer Profiles</CardTitle>
            </CardHeader>
            <CardContent>
              <ProfileSelector />
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>DSP Pipeline</CardTitle>
            </CardHeader>
            <CardContent>
              <DspPipeline />
            </CardContent>
          </Card>
        </>
      )}
    </div>
  );
}
