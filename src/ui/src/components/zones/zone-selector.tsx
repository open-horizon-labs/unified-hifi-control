import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import type { Zone } from '@/types';

interface ZoneSelectorProps {
  zones: Zone[];
  selectedZoneId: string | null;
  onSelect: (zoneId: string | null) => void;
}

export function ZoneSelector({ zones, selectedZoneId, onSelect }: ZoneSelectorProps) {
  return (
    <Select
      value={selectedZoneId || ''}
      onValueChange={(value) => onSelect(value || null)}
    >
      <SelectTrigger className="w-full max-w-xs">
        <SelectValue placeholder="Select a zone" />
      </SelectTrigger>
      <SelectContent>
        {zones.map((zone) => (
          <SelectItem key={zone.zone_id} value={zone.zone_id}>
            {zone.display_name}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
