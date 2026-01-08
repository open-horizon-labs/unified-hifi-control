import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { api } from '@/lib/api';

export function useStatus() {
  return useQuery({
    queryKey: ['status'],
    queryFn: api.getStatus,
    refetchInterval: 4000,
  });
}

export function useZones() {
  const { data: status, ...rest } = useStatus();
  const zones = status?.zones ? Object.values(status.zones) : [];
  return { data: zones, ...rest };
}

export function usePlaybackCommand() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ command, zoneId, outputId }: { command: string; zoneId: string; outputId?: string }) =>
      api.sendCommand(command, zoneId, outputId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['status'] });
    },
  });
}
