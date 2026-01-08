# shadcn/ui Migration Specification

## Executive Summary

This document outlines the migration of the Unified HiFi Control web interface from vanilla JavaScript/HTML/CSS (currently embedded in `src/knobs/routes.js`) to a modern React + TypeScript frontend using shadcn/ui components and Tailwind CSS.

**Current State:**
- Vanilla JavaScript with no framework
- ~1,600 lines of HTML/CSS/JS embedded in Express route handlers
- CSS variable-based theming (light/dark/black)
- No build pipeline for frontend
- 4 main pages: Control, Zone, Knobs, Settings

**Target State:**
- React 18+ with TypeScript
- Vite as the build tool
- shadcn/ui component library with Tailwind CSS
- Separate frontend codebase in `/src/ui`
- API-first architecture with existing Express endpoints

---

## 1. Technology Stack

### Frontend Framework
| Technology | Version | Purpose |
|------------|---------|---------|
| React | 18.3+ | UI framework |
| TypeScript | 5.4+ | Type safety |
| Vite | 5.4+ | Build tool & dev server |
| React Router | 6.x | Client-side routing |

### UI & Styling
| Technology | Version | Purpose |
|------------|---------|---------|
| shadcn/ui | latest | Component library |
| Tailwind CSS | 3.4+ | Utility-first CSS |
| Radix UI | latest | Accessible primitives (via shadcn) |
| Lucide React | latest | Icon library |
| class-variance-authority | latest | Component variants |
| clsx + tailwind-merge | latest | Conditional classes |

### State & Data Fetching
| Technology | Version | Purpose |
|------------|---------|---------|
| TanStack Query | 5.x | Server state management |
| Zustand | 4.x | Client state (optional) |

---

## 2. Project Structure

```
src/ui/
├── public/
│   └── favicon.ico
├── src/
│   ├── components/
│   │   ├── ui/                    # shadcn/ui components
│   │   │   ├── button.tsx
│   │   │   ├── card.tsx
│   │   │   ├── dialog.tsx
│   │   │   ├── dropdown-menu.tsx
│   │   │   ├── input.tsx
│   │   │   ├── label.tsx
│   │   │   ├── select.tsx
│   │   │   ├── slider.tsx
│   │   │   ├── switch.tsx
│   │   │   ├── table.tsx
│   │   │   ├── tabs.tsx
│   │   │   └── toast.tsx
│   │   ├── layout/
│   │   │   ├── navbar.tsx
│   │   │   ├── page-header.tsx
│   │   │   └── root-layout.tsx
│   │   ├── zones/
│   │   │   ├── zone-card.tsx
│   │   │   ├── zone-grid.tsx
│   │   │   ├── zone-controls.tsx
│   │   │   └── album-artwork.tsx
│   │   ├── knobs/
│   │   │   ├── knob-table.tsx
│   │   │   ├── knob-config-dialog.tsx
│   │   │   └── firmware-panel.tsx
│   │   ├── hqplayer/
│   │   │   ├── dsp-pipeline.tsx
│   │   │   ├── profile-selector.tsx
│   │   │   └── connection-status.tsx
│   │   └── settings/
│   │       ├── hqplayer-config.tsx
│   │       ├── lyrion-config.tsx
│   │       ├── backend-toggles.tsx
│   │       └── status-panel.tsx
│   ├── pages/
│   │   ├── control.tsx            # /control - Multi-zone overview
│   │   ├── zone.tsx               # /zone - Single zone + DSP
│   │   ├── knobs.tsx              # /knobs - Device management
│   │   └── settings.tsx           # /settings - Configuration
│   ├── hooks/
│   │   ├── use-zones.ts           # Zone data fetching
│   │   ├── use-knobs.ts           # Knob data fetching
│   │   ├── use-hqplayer.ts        # HQPlayer status/control
│   │   ├── use-lyrion.ts          # Lyrion status/control
│   │   └── use-playback.ts        # Playback control actions
│   ├── lib/
│   │   ├── api.ts                 # API client
│   │   ├── utils.ts               # Utility functions (cn, etc.)
│   │   └── constants.ts           # Shared constants
│   ├── types/
│   │   ├── zone.ts                # Zone types
│   │   ├── knob.ts                # Knob types
│   │   ├── hqplayer.ts            # HQPlayer types
│   │   └── api.ts                 # API response types
│   ├── styles/
│   │   └── globals.css            # Tailwind base + custom CSS
│   ├── App.tsx                    # Root component with router
│   ├── main.tsx                   # Entry point
│   └── vite-env.d.ts              # Vite types
├── components.json                 # shadcn/ui configuration
├── tailwind.config.ts             # Tailwind configuration
├── tsconfig.json                  # TypeScript configuration
├── vite.config.ts                 # Vite configuration
├── package.json                   # Frontend dependencies
└── index.html                     # HTML entry point
```

---

## 3. Component Mapping

### Current → shadcn/ui Component Mapping

| Current Element | shadcn/ui Component | Notes |
|-----------------|---------------------|-------|
| Navigation bar | Custom + `NavigationMenu` | Responsive nav with mobile menu |
| Zone cards | `Card` + `CardHeader/Content` | With hover states |
| Play/pause buttons | `Button` with variants | Icon buttons |
| Volume +/- buttons | `Button` or `Slider` | Consider slider for volume |
| Dropdown selects | `Select` | Zone/profile selection |
| Modals/dialogs | `Dialog` | Knob configuration |
| Tables | `Table` + components | Knob device list |
| Form inputs | `Input` + `Label` | Settings forms |
| Toggle switches | `Switch` | Backend enable/disable |
| Status messages | `Toast` or `Alert` | Success/error feedback |
| Theme toggle | `DropdownMenu` | Light/dark/black selector |
| Collapsible sections | `Collapsible` | Debug panel |
| Tabs | `Tabs` | Settings sections |

### New Components to Create

| Component | Purpose |
|-----------|---------|
| `ZoneCard` | Zone display with artwork, controls, metadata |
| `ZoneGrid` | Responsive grid layout for zones |
| `PlaybackControls` | Unified play/pause/next/prev buttons |
| `VolumeControl` | Volume slider with +/- buttons |
| `AlbumArtwork` | Image with fallback and loading states |
| `DspPipeline` | HQPlayer DSP parameter controls |
| `KnobConfigDialog` | Full knob configuration modal |
| `ConnectionStatus` | Status indicator with color coding |
| `ThemeToggle` | Theme selector dropdown |

---

## 4. Theme Configuration

### Tailwind Theme Extension

The current CSS variable theme will be mapped to Tailwind's theming system:

```typescript
// tailwind.config.ts
import type { Config } from "tailwindcss";

const config: Config = {
  darkMode: ["class"],
  content: ["./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        background: "hsl(var(--background))",
        foreground: "hsl(var(--foreground))",
        card: {
          DEFAULT: "hsl(var(--card))",
          foreground: "hsl(var(--card-foreground))",
        },
        primary: {
          DEFAULT: "hsl(var(--primary))",
          foreground: "hsl(var(--primary-foreground))",
        },
        secondary: {
          DEFAULT: "hsl(var(--secondary))",
          foreground: "hsl(var(--secondary-foreground))",
        },
        muted: {
          DEFAULT: "hsl(var(--muted))",
          foreground: "hsl(var(--muted-foreground))",
        },
        accent: {
          DEFAULT: "hsl(var(--accent))",
          foreground: "hsl(var(--accent-foreground))",
        },
        destructive: {
          DEFAULT: "hsl(var(--destructive))",
          foreground: "hsl(var(--destructive-foreground))",
        },
        border: "hsl(var(--border))",
        input: "hsl(var(--input))",
        ring: "hsl(var(--ring))",
      },
      borderRadius: {
        lg: "var(--radius)",
        md: "calc(var(--radius) - 2px)",
        sm: "calc(var(--radius) - 4px)",
      },
    },
  },
  plugins: [require("tailwindcss-animate")],
};

export default config;
```

### Theme CSS Variables (globals.css)

```css
@tailwind base;
@tailwind components;
@tailwind utilities;

@layer base {
  :root {
    --background: 0 0% 100%;
    --foreground: 0 0% 20%;
    --card: 0 0% 96%;
    --card-foreground: 0 0% 20%;
    --primary: 122 39% 49%;      /* Green accent (#4CAF50) */
    --primary-foreground: 0 0% 100%;
    --secondary: 0 0% 96%;
    --secondary-foreground: 0 0% 20%;
    --muted: 0 0% 96%;
    --muted-foreground: 0 0% 40%;
    --accent: 122 39% 49%;
    --accent-foreground: 0 0% 100%;
    --destructive: 354 70% 54%;
    --destructive-foreground: 0 0% 100%;
    --border: 0 0% 87%;
    --input: 0 0% 87%;
    --ring: 122 39% 49%;
    --radius: 0.5rem;
  }

  .dark {
    --background: 0 0% 13%;
    --foreground: 0 0% 87%;
    --card: 0 0% 18%;
    --card-foreground: 0 0% 87%;
    --primary: 122 39% 49%;
    --primary-foreground: 0 0% 100%;
    --secondary: 0 0% 22%;
    --secondary-foreground: 0 0% 87%;
    --muted: 0 0% 22%;
    --muted-foreground: 0 0% 60%;
    --accent: 122 39% 49%;
    --accent-foreground: 0 0% 100%;
    --destructive: 354 70% 54%;
    --destructive-foreground: 0 0% 100%;
    --border: 0 0% 30%;
    --input: 0 0% 30%;
    --ring: 122 39% 49%;
  }

  .black {
    --background: 0 0% 0%;
    --foreground: 0 0% 80%;
    --card: 0 0% 8%;
    --card-foreground: 0 0% 80%;
    --primary: 122 39% 49%;
    --primary-foreground: 0 0% 100%;
    --secondary: 0 0% 12%;
    --secondary-foreground: 0 0% 80%;
    --muted: 0 0% 12%;
    --muted-foreground: 0 0% 50%;
    --accent: 122 39% 49%;
    --accent-foreground: 0 0% 100%;
    --destructive: 354 70% 54%;
    --destructive-foreground: 0 0% 100%;
    --border: 0 0% 20%;
    --input: 0 0% 20%;
    --ring: 122 39% 49%;
  }
}
```

---

## 5. API Integration

### API Client Structure

```typescript
// src/lib/api.ts
const API_BASE = import.meta.env.VITE_API_BASE || '';

export const api = {
  // Status
  getStatus: () => fetch(`${API_BASE}/admin/status.json`).then(r => r.json()),

  // Zones
  getZones: () => fetch(`${API_BASE}/roon/zones`).then(r => r.json()),
  getAlbumArt: (zoneId: string) => `${API_BASE}/now_playing/image?zone_id=${zoneId}`,

  // Playback Control
  sendCommand: (command: string, zoneId: string, outputId?: string) =>
    fetch(`${API_BASE}/control`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ command, zone_id: zoneId, output_id: outputId }),
    }).then(r => r.json()),

  // HQPlayer
  getHqpStatus: () => fetch(`${API_BASE}/hqp/status`).then(r => r.json()),
  getHqpProfiles: () => fetch(`${API_BASE}/hqp/profiles`).then(r => r.json()),
  loadHqpProfile: (name: string) =>
    fetch(`${API_BASE}/hqp/profiles/load`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name }),
    }).then(r => r.json()),
  getHqpPipeline: () => fetch(`${API_BASE}/hqp/pipeline`).then(r => r.json()),
  updateHqpPipeline: (settings: HqpPipelineSettings) =>
    fetch(`${API_BASE}/hqp/pipeline`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(settings),
    }).then(r => r.json()),
  configureHqp: (config: HqpConfig) =>
    fetch(`${API_BASE}/hqp/configure`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(config),
    }).then(r => r.json()),

  // Lyrion (LMS)
  getLmsStatus: () => fetch(`${API_BASE}/lms/status`).then(r => r.json()),
  configureLms: (config: LmsConfig) =>
    fetch(`${API_BASE}/lms/configure`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(config),
    }).then(r => r.json()),

  // Knobs
  getKnobs: () => fetch(`${API_BASE}/api/knobs`).then(r => r.json()),
  getKnobConfig: (knobId: string) =>
    fetch(`${API_BASE}/config/${knobId}`).then(r => r.json()),
  updateKnobConfig: (knobId: string, config: KnobConfig) =>
    fetch(`${API_BASE}/config/${knobId}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(config),
    }).then(r => r.json()),

  // Settings
  getSettings: () => fetch(`${API_BASE}/api/settings`).then(r => r.json()),
  updateSettings: (settings: AppSettings) =>
    fetch(`${API_BASE}/api/settings`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(settings),
    }).then(r => r.json()),

  // Firmware
  getFirmwareVersion: () => fetch(`${API_BASE}/firmware/version`).then(r => r.json()),
  fetchFirmware: () =>
    fetch(`${API_BASE}/admin/fetch-firmware`, { method: 'POST' }).then(r => r.json()),
};
```

### React Query Hooks

```typescript
// src/hooks/use-zones.ts
import { useQuery } from '@tanstack/react-query';
import { api } from '@/lib/api';

export function useZones() {
  return useQuery({
    queryKey: ['zones'],
    queryFn: api.getZones,
    refetchInterval: 4000, // Match current polling interval
  });
}

export function useStatus() {
  return useQuery({
    queryKey: ['status'],
    queryFn: api.getStatus,
    refetchInterval: 4000,
  });
}
```

---

## 6. TypeScript Types

```typescript
// src/types/zone.ts
export interface Zone {
  zone_id: string;
  display_name: string;
  state: 'playing' | 'paused' | 'stopped' | 'loading';
  now_playing?: NowPlaying;
  outputs: Output[];
  volume?: VolumeInfo;
}

export interface NowPlaying {
  one_line: { line1: string };
  two_line: { line1: string; line2: string };
  three_line: { line1: string; line2: string; line3: string };
  image_key?: string;
  length?: number;
  seek_position?: number;
}

export interface Output {
  output_id: string;
  display_name: string;
  volume?: VolumeInfo;
}

export interface VolumeInfo {
  type: 'number' | 'db';
  value: number;
  min?: number;
  max?: number;
  step?: number;
  is_muted?: boolean;
}

// src/types/knob.ts
export interface Knob {
  id: string;
  name: string;
  version: string;
  ip: string;
  zone_id?: string;
  battery_level?: number;
  last_seen: string;
  charging?: boolean;
}

export interface KnobConfig {
  device_name?: string;
  display_rotation?: 0 | 180;
  power_timers?: {
    charging: { art_mode: number; dim: number; sleep: number };
    battery: { art_mode: number; dim: number; sleep: number };
  };
  wifi_power_save?: boolean;
  cpu_freq_mhz?: number;
  poll_interval_ms?: number;
}

// src/types/hqplayer.ts
export interface HqpStatus {
  connected: boolean;
  playing?: boolean;
  track?: string;
  artist?: string;
}

export interface HqpProfile {
  name: string;
  description?: string;
}

export interface HqpPipelineSettings {
  mode?: 'PCM' | 'SDM' | 'DSD';
  rate?: string;
  filter?: string;
  shaper?: string;
  dither?: string;
  modulator?: string;
}

export interface HqpConfig {
  host: string;
  port: number;
  web_username?: string;
  web_password?: string;
}

// src/types/api.ts
export interface StatusResponse {
  zones: Record<string, Zone>;
  now_playing: Record<string, NowPlaying>;
  backends: {
    roon: boolean;
    upnp: boolean;
    openhome: boolean;
    lyrion: boolean;
  };
  knobs: Knob[];
}
```

---

## 7. Page Implementations

### Control Page (`/control`)

```tsx
// src/pages/control.tsx
import { useStatus } from '@/hooks/use-zones';
import { ZoneGrid } from '@/components/zones/zone-grid';
import { PageHeader } from '@/components/layout/page-header';

export function ControlPage() {
  const { data: status, isLoading, error } = useStatus();

  if (isLoading) return <LoadingSkeleton />;
  if (error) return <ErrorState error={error} />;

  const zones = Object.values(status?.zones ?? {});
  const groupedZones = groupByProtocol(zones);

  return (
    <div className="container mx-auto p-4 space-y-6">
      <PageHeader title="Playback Control" />

      {Object.entries(groupedZones).map(([protocol, zones]) => (
        <section key={protocol}>
          <h2 className="text-lg font-semibold mb-3 text-muted-foreground">
            {protocol}
          </h2>
          <ZoneGrid zones={zones} />
        </section>
      ))}
    </div>
  );
}
```

### Zone Page (`/zone`)

```tsx
// src/pages/zone.tsx
import { useState } from 'react';
import { useZones, useHqpPipeline } from '@/hooks';
import { ZoneSelector } from '@/components/zones/zone-selector';
import { ZoneCard } from '@/components/zones/zone-card';
import { DspPipeline } from '@/components/hqplayer/dsp-pipeline';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

export function ZonePage() {
  const [selectedZoneId, setSelectedZoneId] = usePersistedState<string | null>(
    'selected-zone',
    null
  );
  const { data: zones } = useZones();
  const { data: hqpStatus } = useHqpStatus();

  const selectedZone = zones?.find(z => z.zone_id === selectedZoneId);

  return (
    <div className="container mx-auto p-4 space-y-6">
      <PageHeader title="Zone Focus" />

      <ZoneSelector
        zones={zones ?? []}
        selectedZoneId={selectedZoneId}
        onSelect={setSelectedZoneId}
      />

      {selectedZone && (
        <ZoneCard zone={selectedZone} size="large" />
      )}

      {hqpStatus?.connected && (
        <Card>
          <CardHeader>
            <CardTitle>HQPlayer DSP Pipeline</CardTitle>
          </CardHeader>
          <CardContent>
            <DspPipeline />
          </CardContent>
        </Card>
      )}
    </div>
  );
}
```

### Knobs Page (`/knobs`)

```tsx
// src/pages/knobs.tsx
import { useKnobs } from '@/hooks/use-knobs';
import { KnobTable } from '@/components/knobs/knob-table';
import { FirmwarePanel } from '@/components/knobs/firmware-panel';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

export function KnobsPage() {
  const { data: knobs, isLoading } = useKnobs();

  return (
    <div className="container mx-auto p-4 space-y-6">
      <PageHeader title="Knob Devices" />

      <Card>
        <CardHeader>
          <CardTitle>Registered Devices</CardTitle>
        </CardHeader>
        <CardContent>
          <KnobTable knobs={knobs ?? []} isLoading={isLoading} />
        </CardContent>
      </Card>

      <FirmwarePanel />
    </div>
  );
}
```

### Settings Page (`/settings`)

```tsx
// src/pages/settings.tsx
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { HqplayerConfig } from '@/components/settings/hqplayer-config';
import { LyrionConfig } from '@/components/settings/lyrion-config';
import { BackendToggles } from '@/components/settings/backend-toggles';
import { StatusPanel } from '@/components/settings/status-panel';

export function SettingsPage() {
  return (
    <div className="container mx-auto p-4 space-y-6">
      <PageHeader title="Settings" />

      <Tabs defaultValue="hqplayer">
        <TabsList>
          <TabsTrigger value="hqplayer">HQPlayer</TabsTrigger>
          <TabsTrigger value="lyrion">Lyrion</TabsTrigger>
          <TabsTrigger value="backends">Backends</TabsTrigger>
          <TabsTrigger value="status">Status</TabsTrigger>
        </TabsList>

        <TabsContent value="hqplayer">
          <HqplayerConfig />
        </TabsContent>
        <TabsContent value="lyrion">
          <LyrionConfig />
        </TabsContent>
        <TabsContent value="backends">
          <BackendToggles />
        </TabsContent>
        <TabsContent value="status">
          <StatusPanel />
        </TabsContent>
      </Tabs>
    </div>
  );
}
```

---

## 8. Build & Deployment Integration

### Vite Configuration

```typescript
// src/ui/vite.config.ts
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'path';

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  build: {
    outDir: '../../dist/ui',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': 'http://localhost:3000',
      '/admin': 'http://localhost:3000',
      '/control': 'http://localhost:3000',
      '/roon': 'http://localhost:3000',
      '/hqp': 'http://localhost:3000',
      '/lms': 'http://localhost:3000',
      '/config': 'http://localhost:3000',
      '/now_playing': 'http://localhost:3000',
      '/firmware': 'http://localhost:3000',
    },
  },
});
```

### Express Integration

Update `src/server/app.js` to serve the built React app:

```javascript
// Add to app.js after API routes
const path = require('path');

// Serve static files from React build
app.use(express.static(path.join(__dirname, '../../dist/ui')));

// Catch-all for React Router (client-side routing)
app.get(['/', '/control', '/zone', '/knobs', '/settings'], (req, res) => {
  res.sendFile(path.join(__dirname, '../../dist/ui/index.html'));
});
```

### Package Scripts

```json
{
  "scripts": {
    "dev": "concurrently \"npm run dev:server\" \"npm run dev:ui\"",
    "dev:server": "nodemon src/server/index.js",
    "dev:ui": "cd src/ui && npm run dev",
    "build": "npm run build:ui && npm run build:server",
    "build:ui": "cd src/ui && npm run build",
    "start": "node src/server/index.js"
  }
}
```

---

## 9. Migration Phases

### Phase 1: Foundation (Week 1-2)
- [ ] Initialize Vite + React + TypeScript in `/src/ui`
- [ ] Install and configure Tailwind CSS
- [ ] Install shadcn/ui and configure `components.json`
- [ ] Set up path aliases and build configuration
- [ ] Create base layout with navbar
- [ ] Implement theme system (light/dark/black)
- [ ] Set up API client and React Query

### Phase 2: Core Components (Week 2-3)
- [ ] Add required shadcn/ui components:
  - Button, Card, Dialog, Input, Label
  - Select, Switch, Table, Tabs, Toast
- [ ] Create shared components:
  - PageHeader, ConnectionStatus, ThemeToggle
- [ ] Implement zone components:
  - ZoneCard, ZoneGrid, AlbumArtwork, PlaybackControls

### Phase 3: Page Implementation (Week 3-4)
- [ ] Implement Control page with zone grid
- [ ] Implement Zone page with DSP pipeline
- [ ] Implement Knobs page with table and config dialog
- [ ] Implement Settings page with all sections

### Phase 4: Polish & Integration (Week 4-5)
- [ ] Add loading states and skeletons
- [ ] Add error boundaries and error states
- [ ] Implement toast notifications
- [ ] Mobile responsiveness testing
- [ ] Accessibility audit (keyboard nav, screen readers)
- [ ] Express integration for production serving
- [ ] Update Docker build process

### Phase 5: Deprecation (Week 5+)
- [ ] Remove old routes from `src/knobs/routes.js`
- [ ] Update documentation
- [ ] Performance optimization (lazy loading, memoization)

---

## 10. Testing Strategy

### Unit Tests
- Component rendering with React Testing Library
- Hook behavior with `@testing-library/react-hooks`
- API client functions with mocked fetch

### Integration Tests
- Page navigation with React Router
- Form submissions
- API interactions

### E2E Tests (optional)
- Playwright or Cypress for critical user flows
- Zone selection and playback control
- Settings configuration

---

## 11. Accessibility Requirements

- All interactive elements must be keyboard accessible
- Focus management for dialogs and modals
- ARIA labels for icon-only buttons
- Color contrast ratios meeting WCAG 2.1 AA
- Screen reader announcements for state changes
- Reduced motion support

---

## 12. Open Questions

1. **Routing strategy**: Keep hash-based routing for simplicity or use browser history?
   - Recommendation: Browser history with Express catch-all

2. **Volume control**: Buttons only or add slider?
   - Recommendation: Slider with +/- buttons for precision

3. **Real-time updates**: Continue polling or implement WebSocket?
   - Recommendation: Keep polling initially, consider WebSocket later

4. **State persistence**: localStorage sufficient or need server-side?
   - Recommendation: localStorage for UI preferences, server for critical settings

5. **Monorepo structure**: Keep UI in same repo or separate?
   - Recommendation: Keep in same repo under `/src/ui` for simplicity

---

## 13. Dependencies

### Frontend `package.json`

```json
{
  "name": "unified-hifi-control-ui",
  "private": true,
  "version": "0.0.1",
  "type": "module",
  "scripts": {
    "dev": "vite",
    "build": "tsc && vite build",
    "preview": "vite preview",
    "lint": "eslint . --ext ts,tsx"
  },
  "dependencies": {
    "@radix-ui/react-dialog": "^1.0.5",
    "@radix-ui/react-dropdown-menu": "^2.0.6",
    "@radix-ui/react-label": "^2.0.2",
    "@radix-ui/react-select": "^2.0.0",
    "@radix-ui/react-slider": "^1.1.2",
    "@radix-ui/react-slot": "^1.0.2",
    "@radix-ui/react-switch": "^1.0.3",
    "@radix-ui/react-tabs": "^1.0.4",
    "@radix-ui/react-toast": "^1.1.5",
    "@tanstack/react-query": "^5.28.0",
    "class-variance-authority": "^0.7.0",
    "clsx": "^2.1.0",
    "lucide-react": "^0.356.0",
    "react": "^18.2.0",
    "react-dom": "^18.2.0",
    "react-router-dom": "^6.22.3",
    "tailwind-merge": "^2.2.1",
    "tailwindcss-animate": "^1.0.7"
  },
  "devDependencies": {
    "@types/react": "^18.2.66",
    "@types/react-dom": "^18.2.22",
    "@typescript-eslint/eslint-plugin": "^7.2.0",
    "@typescript-eslint/parser": "^7.2.0",
    "@vitejs/plugin-react": "^4.2.1",
    "autoprefixer": "^10.4.18",
    "eslint": "^8.57.0",
    "eslint-plugin-react-hooks": "^4.6.0",
    "eslint-plugin-react-refresh": "^0.4.6",
    "postcss": "^8.4.35",
    "tailwindcss": "^3.4.1",
    "typescript": "^5.4.2",
    "vite": "^5.1.6"
  }
}
```

---

## Appendix A: shadcn/ui Components Checklist

Required components to install:

```bash
npx shadcn@latest add button
npx shadcn@latest add card
npx shadcn@latest add dialog
npx shadcn@latest add dropdown-menu
npx shadcn@latest add input
npx shadcn@latest add label
npx shadcn@latest add select
npx shadcn@latest add slider
npx shadcn@latest add switch
npx shadcn@latest add table
npx shadcn@latest add tabs
npx shadcn@latest add toast
npx shadcn@latest add collapsible
npx shadcn@latest add skeleton
npx shadcn@latest add alert
```

---

## Appendix B: Current Routes to Deprecate

After migration is complete, remove these routes from `src/knobs/routes.js`:

| Route | Lines | Replacement |
|-------|-------|-------------|
| `GET /` | Redirect | React Router |
| `GET /control` | 504-666 | `pages/control.tsx` |
| `GET /zone` | 668-905 | `pages/zone.tsx` |
| `GET /knobs` | 908-1105 | `pages/knobs.tsx` |
| `GET /settings` | 1138-1420 | `pages/settings.tsx` |

Keep all API routes (`/admin/*`, `/api/*`, `/hqp/*`, `/lms/*`, `/config/*`, etc.) unchanged.
