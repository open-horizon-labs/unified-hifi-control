import { useMemo } from 'react';
import { useStatus } from '@/hooks';
import { PageHeader } from '@/components/layout';
import { ZoneCard } from '@/components/zones';
import { Skeleton } from '@/components/ui/skeleton';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { AlertCircle } from 'lucide-react';
import type { Zone } from '@/types';

function groupZonesBySource(zones: Zone[]): Record<string, Zone[]> {
  const groups: Record<string, Zone[]> = {};

  for (const zone of zones) {
    const source = zone.source || 'Other';
    if (!groups[source]) {
      groups[source] = [];
    }
    groups[source].push(zone);
  }

  // Sort sources: Roon first, then alphabetically
  const sortedGroups: Record<string, Zone[]> = {};
  const sources = Object.keys(groups).sort((a, b) => {
    if (a === 'Roon') return -1;
    if (b === 'Roon') return 1;
    return a.localeCompare(b);
  });

  for (const source of sources) {
    sortedGroups[source] = groups[source];
  }

  return sortedGroups;
}

function LoadingSkeleton() {
  return (
    <div className="space-y-6">
      <div className="space-y-3">
        <Skeleton className="h-6 w-24" />
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {[1, 2, 3].map((i) => (
            <Skeleton key={i} className="h-32" />
          ))}
        </div>
      </div>
    </div>
  );
}

export function ControlPage() {
  const { data: status, isLoading, error } = useStatus();

  const groupedZones = useMemo(() => {
    if (!status?.zones) return {};
    const zones = Object.values(status.zones);
    return groupZonesBySource(zones);
  }, [status?.zones]);

  if (isLoading) {
    return (
      <div className="container mx-auto p-4 space-y-6">
        <PageHeader title="Playback Control" />
        <LoadingSkeleton />
      </div>
    );
  }

  if (error) {
    return (
      <div className="container mx-auto p-4 space-y-6">
        <PageHeader title="Playback Control" />
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>
            Failed to load zones: {error.message}
          </AlertDescription>
        </Alert>
      </div>
    );
  }

  const hasZones = Object.keys(groupedZones).length > 0;

  return (
    <div className="container mx-auto p-4 space-y-6">
      <PageHeader
        title="Playback Control"
        description={hasZones ? undefined : 'No zones available'}
      />

      {Object.entries(groupedZones).map(([source, zones]) => (
        <section key={source} className="space-y-3">
          <h2 className="text-lg font-semibold text-muted-foreground">
            {source}
          </h2>
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {zones.map((zone) => (
              <ZoneCard key={zone.zone_id} zone={zone} />
            ))}
          </div>
        </section>
      ))}
    </div>
  );
}
