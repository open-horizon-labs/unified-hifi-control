import { NavLink } from 'react-router-dom';
import { Play, Focus, Disc3, Settings } from 'lucide-react';
import { cn } from '@/lib/utils';
import { ThemeToggle } from './theme-toggle';
import { useSettings } from '@/hooks';

const navLinkClass = ({ isActive }: { isActive: boolean }) =>
  cn(
    'flex items-center gap-2 px-3 py-2 rounded-md text-sm font-medium transition-colors',
    isActive
      ? 'bg-primary text-primary-foreground'
      : 'text-muted-foreground hover:text-foreground hover:bg-accent'
  );

export function Navbar() {
  const { data: settings } = useSettings();
  const hideKnobs = settings?.hideKnobsPage;

  return (
    <nav className="border-b bg-card">
      <div className="container mx-auto px-4">
        <div className="flex h-14 items-center justify-between">
          <div className="flex items-center gap-1">
            <NavLink to="/control" className={navLinkClass}>
              <Play className="h-4 w-4" />
              <span className="hidden sm:inline">Control</span>
            </NavLink>
            <NavLink to="/zone" className={navLinkClass}>
              <Focus className="h-4 w-4" />
              <span className="hidden sm:inline">Zone</span>
            </NavLink>
            {!hideKnobs && (
              <NavLink to="/knobs" className={navLinkClass}>
                <Disc3 className="h-4 w-4" />
                <span className="hidden sm:inline">Knobs</span>
              </NavLink>
            )}
            <NavLink to="/settings" className={navLinkClass}>
              <Settings className="h-4 w-4" />
              <span className="hidden sm:inline">Settings</span>
            </NavLink>
          </div>
          <ThemeToggle />
        </div>
      </div>
    </nav>
  );
}
