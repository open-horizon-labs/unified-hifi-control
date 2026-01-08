import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { api } from '@/lib/api';
import type { HqpConfig, HqpPipelineSettings } from '@/types';

export function useHqpStatus() {
  return useQuery({
    queryKey: ['hqp-status'],
    queryFn: api.getHqpStatus,
    refetchInterval: 5000,
  });
}

export function useHqpProfiles() {
  return useQuery({
    queryKey: ['hqp-profiles'],
    queryFn: api.getHqpProfiles,
  });
}

export function useLoadHqpProfile() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (name: string) => api.loadHqpProfile(name),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['hqp-status'] });
      queryClient.invalidateQueries({ queryKey: ['hqp-pipeline'] });
    },
  });
}

export function useHqpPipeline() {
  return useQuery({
    queryKey: ['hqp-pipeline'],
    queryFn: api.getHqpPipeline,
  });
}

export function useUpdateHqpPipeline() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (settings: HqpPipelineSettings) => api.updateHqpPipeline(settings),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['hqp-pipeline'] });
    },
  });
}

export function useConfigureHqp() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (config: HqpConfig) => api.configureHqp(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['hqp-status'] });
    },
  });
}
