import { useState } from 'react';
import { Music } from 'lucide-react';
import { cn } from '@/lib/utils';
import { api } from '@/lib/api';

interface AlbumArtworkProps {
  zoneId: string;
  size?: 'sm' | 'md' | 'lg';
  className?: string;
}

const sizeClasses = {
  sm: 'w-12 h-12',
  md: 'w-16 h-16',
  lg: 'w-28 h-28',
};

export function AlbumArtwork({ zoneId, size = 'md', className }: AlbumArtworkProps) {
  const [hasError, setHasError] = useState(false);
  const [isLoading, setIsLoading] = useState(true);

  const handleError = () => {
    setHasError(true);
    setIsLoading(false);
  };

  const handleLoad = () => {
    setIsLoading(false);
  };

  return (
    <div
      className={cn(
        'rounded-md overflow-hidden bg-muted flex items-center justify-center flex-shrink-0',
        sizeClasses[size],
        className
      )}
    >
      {hasError ? (
        <Music className="w-1/2 h-1/2 text-muted-foreground" />
      ) : (
        <>
          {isLoading && (
            <div className="absolute inset-0 flex items-center justify-center">
              <Music className="w-1/2 h-1/2 text-muted-foreground animate-pulse" />
            </div>
          )}
          <img
            src={api.getAlbumArtUrl(zoneId)}
            alt="Album artwork"
            className={cn('w-full h-full object-cover', isLoading && 'opacity-0')}
            onError={handleError}
            onLoad={handleLoad}
          />
        </>
      )}
    </div>
  );
}
