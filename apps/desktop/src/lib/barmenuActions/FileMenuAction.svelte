<script lang="ts">
	import { listen } from '$lib/backend/ipc';
	import { ProjectsService } from '$lib/backend/projects';
	import { createKeybind } from '$lib/utils/hotkeys';
	import { getContext } from '@gitbutler/shared/context';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';

	const projectsService = getContext(ProjectsService);

	onMount(() => {
		const unsubscribeAddLocalRepo = listen<string>(
			'menu://file/add-local-repo/clicked',
			async () => {
				await projectsService.addProject();
			}
		);

		const unsubscribeCloneRepo = listen<string>('menu://file/clone-repo/clicked', async () => {
			goto('/onboarding/clone');
		});

		return async () => {
			unsubscribeAddLocalRepo();
			unsubscribeCloneRepo();
		};
	});

	const handleKeyDown = createKeybind({
		'$mod+O': async () => {
			await projectsService.addProject();
		},
		'$mod+Shift+O': async () => {
			goto('/onboarding/clone');
		}
	});
</script>

<svelte:window onkeydown={handleKeyDown} />
