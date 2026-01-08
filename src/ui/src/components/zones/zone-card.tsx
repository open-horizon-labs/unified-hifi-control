import { Card, CardContent } from '@/components/ui/card';
import { cn } from '@/lib/utils';
import { AlbumArtwork } from './album-artwork';
import { PlaybackControls } from './playback-controls';
import type { Zone } from '@/types';

interface ZoneCardProps {
  zone: Zone;
  size?: 'sm' | 'lg';
  selected?: boolean;
  onClick?: () => void;
}

export function ZoneCard({ zone, size = 'sm', selected, onClick }: ZoneCardProps) {
  const nowPlaying = zone.now_playing;
  const isLarge = size === 'lg';

  const getDisplayText = () => {
    if (!nowPlaying) {
      return { line1: zone.display_name, line2: 'Not playing' };
    }
    if (nowPlaying.two_line) {
      return { line1: nowPlaying.two_line.line1, line2: nowPlaying.two_line.line2 };
    }
    if (nowPlaying.one_line) {
      return { line1: nowPlaying.one_line.line1, line2: '' };
    }
    return { line1: zone.display_name, line2: 'Playing' };
  };

  const { line1, line2 } = getDisplayText();

  return (
    <Card
      className={cn(
        'transition-colors',
        onClick && 'cursor-pointer hover:bg-accent/50',
        selected && 'ring-2 ring-primary'
      )}
      onClick={onClick}
    >
      <CardContent className={cn('p-4', isLarge && 'p-6')}>
        <div className={cn('flex gap-4', isLarge && 'flex-col items-center text-center')}>
          <AlbumArtwork
            zoneId={zone.zone_id}
            size={isLarge ? 'lg' : 'md'}
          />

          <div className={cn('flex-1 min-w-0', isLarge && 'w-full')}>
            <p className="text-xs text-muted-foreground mb-1">
              {zone.display_name}
            </p>
            <p className={cn('font-medium truncate', isLarge && 'text-lg')}>
              {line1}
            </p>
            {line2 && (
              <p className="text-sm text-muted-foreground truncate">
                {line2}
              </p>
            )}

            <div className={cn('mt-3', isLarge && 'mt-4')}>
              <PlaybackControls zone={zone} size={isLarge ? 'md' : 'sm'} />
            </div>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}
