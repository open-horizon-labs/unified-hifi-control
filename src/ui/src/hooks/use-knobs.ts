import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { api } from '@/lib/api';
import type { KnobConfig } from '@/types';

export function useKnobs() {
  return useQuery({
    queryKey: ['knobs'],
    queryFn: api.getKnobs,
    refetchInterval: 10000,
  });
}

export function useKnobConfig(knobId: string | null) {
  return useQuery({
    queryKey: ['knob-config', knobId],
    queryFn: () => (knobId ? api.getKnobConfig(knobId) : Promise.resolve(null)),
    enabled: !!knobId,
  });
}

export function useUpdateKnobConfig() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ knobId, config }: { knobId: string; config: KnobConfig }) =>
      api.updateKnobConfig(knobId, config),
    onSuccess: (_, { knobId }) => {
      queryClient.invalidateQueries({ queryKey: ['knobs'] });
      queryClient.invalidateQueries({ queryKey: ['knob-config', knobId] });
    },
  });
}

export function useFirmware() {
  return useQuery({
    queryKey: ['firmware'],
    queryFn: api.getFirmwareVersion,
  });
}

export function useFetchFirmware() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: api.fetchFirmware,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['firmware'] });
    },
  });
}
