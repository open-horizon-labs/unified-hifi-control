import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { api } from '@/lib/api';
import type { AppSettings, LmsConfig } from '@/types';

export function useSettings() {
  return useQuery({
    queryKey: ['settings'],
    queryFn: api.getSettings,
  });
}

export function useUpdateSettings() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (settings: AppSettings) => api.updateSettings(settings),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['settings'] });
    },
  });
}

export function useLmsStatus() {
  return useQuery({
    queryKey: ['lms-status'],
    queryFn: api.getLmsStatus,
    refetchInterval: 5000,
  });
}

export function useConfigureLms() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (config: LmsConfig) => api.configureLms(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['lms-status'] });
    },
  });
}
