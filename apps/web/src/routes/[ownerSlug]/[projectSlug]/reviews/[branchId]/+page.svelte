<script lang="ts">
	import type { ProjectReviewParameters } from '$lib/project/types';

	interface Props {
		data: ProjectReviewParameters;
	}

	let { data }: Props = $props();
</script>

<h2>Review page: {data.ownerSlug}/{data.projectSlug} {data.branchId}</h2>
