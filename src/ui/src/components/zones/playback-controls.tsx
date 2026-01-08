import { Play, Pause, SkipBack, SkipForward, Volume2, VolumeX } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { usePlaybackCommand } from '@/hooks';
import type { Zone } from '@/types';

interface PlaybackControlsProps {
  zone: Zone;
  size?: 'sm' | 'md';
}

export function PlaybackControls({ zone, size = 'md' }: PlaybackControlsProps) {
  const { mutate: sendCommand, isPending } = usePlaybackCommand();

  const isPlaying = zone.state === 'playing';
  const iconSize = size === 'sm' ? 'h-4 w-4' : 'h-5 w-5';
  const buttonSize = size === 'sm' ? 'icon' : 'icon';

  const handleCommand = (command: string) => {
    sendCommand({ command, zoneId: zone.zone_id });
  };

  return (
    <div className="flex items-center gap-1">
      <Button
        variant="ghost"
        size={buttonSize}
        onClick={() => handleCommand('previous')}
        disabled={isPending}
        title="Previous track"
      >
        <SkipBack className={iconSize} />
      </Button>

      <Button
        variant="ghost"
        size={buttonSize}
        onClick={() => handleCommand(isPlaying ? 'pause' : 'play')}
        disabled={isPending}
        title={isPlaying ? 'Pause' : 'Play'}
      >
        {isPlaying ? (
          <Pause className={iconSize} />
        ) : (
          <Play className={iconSize} />
        )}
      </Button>

      <Button
        variant="ghost"
        size={buttonSize}
        onClick={() => handleCommand('next')}
        disabled={isPending}
        title="Next track"
      >
        <SkipForward className={iconSize} />
      </Button>

      <Button
        variant="ghost"
        size={buttonSize}
        onClick={() => handleCommand('volume_down')}
        disabled={isPending}
        title="Volume down"
      >
        <VolumeX className={iconSize} />
      </Button>

      <Button
        variant="ghost"
        size={buttonSize}
        onClick={() => handleCommand('volume_up')}
        disabled={isPending}
        title="Volume up"
      >
        <Volume2 className={iconSize} />
      </Button>
    </div>
  );
}
